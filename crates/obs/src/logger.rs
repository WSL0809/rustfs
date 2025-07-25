// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::sinks::Sink;
use crate::{
    AppConfig, AuditLogEntry, BaseLogEntry, ConsoleLogEntry, GlobalError, OtelConfig, ServerLogEntry, UnifiedLogEntry, sinks,
};
use rustfs_config::{APP_NAME, ENVIRONMENT, SERVICE_VERSION};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{Mutex, OnceCell};
use tracing_core::Level;

// Add the global instance at the module level
static GLOBAL_LOGGER: OnceCell<Arc<Mutex<Logger>>> = OnceCell::const_new();

/// Server log processor
#[derive(Debug)]
pub struct Logger {
    sender: Sender<UnifiedLogEntry>, // Log sending channel
    queue_capacity: usize,
}

impl Logger {
    /// Create a new Logger instance
    /// Returns Logger and corresponding Receiver
    pub fn new(config: &AppConfig) -> (Self, Receiver<UnifiedLogEntry>) {
        // Get queue capacity from configuration, or use default values 10000
        let queue_capacity = config.logger.as_ref().and_then(|l| l.queue_capacity).unwrap_or(10000);
        let (sender, receiver) = mpsc::channel(queue_capacity);
        (Logger { sender, queue_capacity }, receiver)
    }

    /// get the queue capacity
    /// This function returns the queue capacity.
    /// # Returns
    /// The queue capacity
    /// # Example
    /// ```
    /// use rustfs_obs::Logger;
    /// async fn example(logger: &Logger) {
    ///    let _ = logger.get_queue_capacity();
    /// }
    /// ```
    pub fn get_queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    /// Log a server entry
    #[tracing::instrument(skip(self), fields(log_source = "logger_server"))]
    pub async fn log_server_entry(&self, entry: ServerLogEntry) -> Result<(), GlobalError> {
        self.log_entry(UnifiedLogEntry::Server(entry)).await
    }

    /// Log an audit entry
    #[tracing::instrument(skip(self), fields(log_source = "logger_audit"))]
    pub async fn log_audit_entry(&self, entry: AuditLogEntry) -> Result<(), GlobalError> {
        self.log_entry(UnifiedLogEntry::Audit(Box::new(entry))).await
    }

    /// Log a console entry
    #[tracing::instrument(skip(self), fields(log_source = "logger_console"))]
    pub async fn log_console_entry(&self, entry: ConsoleLogEntry) -> Result<(), GlobalError> {
        self.log_entry(UnifiedLogEntry::Console(entry)).await
    }

    /// Asynchronous logging of unified log entries
    #[tracing::instrument(skip_all, fields(log_source = "logger"))]
    pub async fn log_entry(&self, entry: UnifiedLogEntry) -> Result<(), GlobalError> {
        // Extract information for tracing based on entry type
        match &entry {
            UnifiedLogEntry::Server(server) => {
                tracing::Span::current()
                    .record("log_level", server.level.0.as_str())
                    .record("log_message", server.base.message.as_deref().unwrap_or("log message not set"))
                    .record("source", &server.source);

                // Generate tracing event based on log level
                match server.level.0 {
                    Level::ERROR => {
                        tracing::error!(target: "server_logs", message = %server.base.message.as_deref().unwrap_or(""));
                    }
                    Level::WARN => {
                        tracing::warn!(target: "server_logs", message = %server.base.message.as_deref().unwrap_or(""));
                    }
                    Level::INFO => {
                        tracing::info!(target: "server_logs", message = %server.base.message.as_deref().unwrap_or(""));
                    }
                    Level::DEBUG => {
                        tracing::debug!(target: "server_logs", message = %server.base.message.as_deref().unwrap_or(""));
                    }
                    Level::TRACE => {
                        tracing::trace!(target: "server_logs", message = %server.base.message.as_deref().unwrap_or(""));
                    }
                }
            }
            UnifiedLogEntry::Audit(audit) => {
                tracing::info!(
                    target: "audit_logs",
                    event = %audit.event,
                    api = %audit.api.name.as_deref().unwrap_or("unknown"),
                    message = %audit.base.message.as_deref().unwrap_or("")
                );
            }
            UnifiedLogEntry::Console(console) => {
                let level_str = match console.level {
                    crate::LogKind::Info => "INFO",
                    crate::LogKind::Warning => "WARN",
                    crate::LogKind::Error => "ERROR",
                    crate::LogKind::Fatal => "FATAL",
                };

                tracing::info!(
                    target: "console_logs",
                    level = %level_str,
                    node = %console.node_name,
                    message = %console.console_msg
                );
            }
        }

        // Send logs to async queue with improved error handling
        match self.sender.try_send(entry) {
            Ok(_) => Ok(()),
            Err(mpsc::error::TrySendError::Full(entry)) => {
                // Processing strategy when queue is full
                tracing::warn!("Log queue full, applying backpressure");
                match tokio::time::timeout(std::time::Duration::from_millis(500), self.sender.send(entry)).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(_)) => Err(GlobalError::SendFailed("Channel closed")),
                    Err(_) => Err(GlobalError::Timeout("Queue backpressure timeout")),
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(GlobalError::SendFailed("Logger channel closed")),
        }
    }

    /// Write log with context information
    /// This function writes log messages with context information.
    ///
    /// # Parameters
    /// - `message`: Message to be logged
    /// - `source`: Source of the log
    /// - `request_id`: Request ID
    /// - `user_id`: User ID
    /// - `fields`: Additional fields
    ///
    /// # Returns
    /// Result indicating whether the operation was successful
    ///
    /// # Example
    /// ```
    /// use tracing_core::Level;
    /// use rustfs_obs::Logger;
    ///
    /// async fn example(logger: &Logger) {
    ///    let _ = logger.write_with_context("This is an information message", "example",Level::INFO, Some("req-12345".to_string()), Some("user-6789".to_string()), vec![("endpoint".to_string(), "/api/v1/data".to_string())]).await;
    /// }
    pub async fn write_with_context(
        &self,
        message: &str,
        source: &str,
        level: Level,
        request_id: Option<String>,
        user_id: Option<String>,
        fields: Vec<(String, String)>,
    ) -> Result<(), GlobalError> {
        let base = BaseLogEntry::new().message(Some(message.to_string())).request_id(request_id);

        let server_entry = ServerLogEntry::new(level, source.to_string())
            .user_id(user_id)
            .fields(fields)
            .with_base(base);

        self.log_server_entry(server_entry).await
    }

    /// Write log
    /// This function writes log messages.
    /// # Parameters
    /// - `message`: Message to be logged
    /// - `source`: Source of the log
    /// - `level`: Log level
    ///
    /// # Returns
    /// Result indicating whether the operation was successful
    ///
    /// # Example
    /// ```
    /// use rustfs_obs::Logger;
    /// use tracing_core::Level;
    ///
    /// async fn example(logger: &Logger) {
    ///   let _ = logger.write("This is an information message", "example", Level::INFO).await;
    /// }
    /// ```
    pub async fn write(&self, message: &str, source: &str, level: Level) -> Result<(), GlobalError> {
        self.write_with_context(message, source, level, None, None, Vec::new()).await
    }

    /// Shutdown the logger
    /// This function shuts down the logger.
    ///
    /// # Returns
    /// Result indicating whether the operation was successful
    ///
    /// # Example
    /// ```
    /// use rustfs_obs::Logger;
    ///
    /// async fn example(logger: Logger) {
    ///  let _ = logger.shutdown().await;
    /// }
    /// ```
    pub async fn shutdown(self) -> Result<(), GlobalError> {
        drop(self.sender); //Close the sending end so that the receiver knows that there is no new message
        Ok(())
    }
}

/// Start the log module
/// This function starts the log module.
/// It initializes the logger and starts the worker to process logs.
/// # Parameters
/// - `config`: Configuration information
/// - `sinks`: A vector of Sink instances
/// # Returns
/// The global logger instance
/// # Example
/// ```no_run
/// use rustfs_obs::{AppConfig, start_logger};
///
/// let config = AppConfig::default();
/// let sinks = vec![];
/// let logger = start_logger(&config, sinks);
/// ```
pub fn start_logger(config: &AppConfig, sinks: Vec<Arc<dyn Sink>>) -> Logger {
    let (logger, receiver) = Logger::new(config);
    tokio::spawn(crate::worker::start_worker(receiver, sinks));
    logger
}

/// Initialize the global logger instance
/// This function initializes the global logger instance and returns a reference to it.
/// If the logger has been initialized before, it will return the existing logger instance.
///
/// # Parameters
/// - `config`: Configuration information
/// - `sinks`: A vector of Sink instances
///
/// # Returns
/// A reference to the global logger instance
///
/// # Example
/// ```
/// use rustfs_obs::{AppConfig,init_global_logger};
///
/// let config = AppConfig::default();
/// let logger = init_global_logger(&config);
/// ```
pub async fn init_global_logger(config: &AppConfig) -> Arc<Mutex<Logger>> {
    let sinks = sinks::create_sinks(config).await;
    let logger = Arc::new(Mutex::new(start_logger(config, sinks)));
    GLOBAL_LOGGER.set(logger.clone()).expect("Logger already initialized");
    logger
}

/// Get the global logger instance
///
/// This function returns a reference to the global logger instance.
///
/// # Returns
/// A reference to the global logger instance
///
/// # Example
/// ```no_run
/// use rustfs_obs::get_global_logger;
///
/// let logger = get_global_logger();
/// ```
pub fn get_global_logger() -> &'static Arc<Mutex<Logger>> {
    GLOBAL_LOGGER.get().expect("Logger not initialized")
}

/// Log information
/// This function logs information messages.
///
/// # Parameters
/// - `message`: Message to be logged
/// - `source`: Source of the log
///
/// # Returns
/// Result indicating whether the operation was successful
///
/// # Example
/// ```no_run
/// use rustfs_obs::log_info;
///
/// async fn example() {
///    let _ = log_info("This is an information message", "example").await;
/// }
/// ```
pub async fn log_info(message: &str, source: &str) -> Result<(), GlobalError> {
    get_global_logger().lock().await.write(message, source, Level::INFO).await
}

/// Log error
/// This function logs error messages.
/// # Parameters
/// - `message`: Message to be logged
/// - `source`: Source of the log
/// # Returns
/// Result indicating whether the operation was successful
/// # Example
/// ```no_run
/// use rustfs_obs::log_error;
///
/// async fn example() {
///     let _ = log_error("This is an error message", "example").await;
/// }
pub async fn log_error(message: &str, source: &str) -> Result<(), GlobalError> {
    get_global_logger().lock().await.write(message, source, Level::ERROR).await
}

/// Log warning
/// This function logs warning messages.
/// # Parameters
/// - `message`: Message to be logged
/// - `source`: Source of the log
/// # Returns
/// Result indicating whether the operation was successful
///
/// # Example
/// ```no_run
/// use rustfs_obs::log_warn;
///
/// async fn example() {
///     let _ = log_warn("This is a warning message", "example").await;
/// }
/// ```
pub async fn log_warn(message: &str, source: &str) -> Result<(), GlobalError> {
    get_global_logger().lock().await.write(message, source, Level::WARN).await
}

/// Log debug
/// This function logs debug messages.
/// # Parameters
/// - `message`: Message to be logged
/// - `source`: Source of the log
/// # Returns
/// Result indicating whether the operation was successful
///
/// # Example
/// ```no_run
/// use rustfs_obs::log_debug;
///
/// async fn example() {
///     let _ = log_debug("This is a debug message", "example").await;
/// }
/// ```
pub async fn log_debug(message: &str, source: &str) -> Result<(), GlobalError> {
    get_global_logger().lock().await.write(message, source, Level::DEBUG).await
}

/// Log trace
/// This function logs trace messages.
/// # Parameters
/// - `message`: Message to be logged
/// - `source`: Source of the log
///
/// # Returns
/// Result indicating whether the operation was successful
///
/// # Example
/// ```no_run
/// use rustfs_obs::log_trace;
///
/// async fn example() {
///    let _ = log_trace("This is a trace message", "example").await;
/// }
/// ```
pub async fn log_trace(message: &str, source: &str) -> Result<(), GlobalError> {
    get_global_logger().lock().await.write(message, source, Level::TRACE).await
}

/// Log with context information
/// This function logs messages with context information.
/// # Parameters
/// - `message`: Message to be logged
/// - `source`: Source of the log
/// - `level`: Log level
/// - `request_id`: Request ID
/// - `user_id`: User ID
/// - `fields`: Additional fields
/// # Returns
/// Result indicating whether the operation was successful
/// # Example
/// ```no_run
/// use tracing_core::Level;
/// use rustfs_obs::log_with_context;
///
/// async fn example() {
///    let _ = log_with_context("This is an information message", "example", Level::INFO, Some("req-12345".to_string()), Some("user-6789".to_string()), vec![("endpoint".to_string(), "/api/v1/data".to_string())]).await;
/// }
/// ```
pub async fn log_with_context(
    message: &str,
    source: &str,
    level: Level,
    request_id: Option<String>,
    user_id: Option<String>,
    fields: Vec<(String, String)>,
) -> Result<(), GlobalError> {
    get_global_logger()
        .lock()
        .await
        .write_with_context(message, source, level, request_id, user_id, fields)
        .await
}

/// Log initialization status
#[derive(Debug)]
pub(crate) struct InitLogStatus {
    pub timestamp: SystemTime,
    pub service_name: String,
    pub version: String,
    pub environment: String,
}

impl Default for InitLogStatus {
    fn default() -> Self {
        Self {
            timestamp: SystemTime::now(),
            service_name: String::from(APP_NAME),
            version: SERVICE_VERSION.to_string(),
            environment: ENVIRONMENT.to_string(),
        }
    }
}

impl InitLogStatus {
    pub fn new_config(config: &OtelConfig) -> Self {
        let config = config.clone();
        let environment = config.environment.unwrap_or(ENVIRONMENT.to_string());
        let version = config.service_version.unwrap_or(SERVICE_VERSION.to_string());
        Self {
            timestamp: SystemTime::now(),
            service_name: String::from(APP_NAME),
            version,
            environment,
        }
    }

    pub async fn init_start_log(config: &OtelConfig) -> Result<(), GlobalError> {
        let status = Self::new_config(config);
        log_init_state(Some(status)).await
    }
}

/// Log initialization details during system startup
async fn log_init_state(status: Option<InitLogStatus>) -> Result<(), GlobalError> {
    let status = status.unwrap_or_default();

    let base_entry = BaseLogEntry::new()
        .timestamp(chrono::DateTime::from(status.timestamp))
        .message(Some(format!(
            "Service initialization started - {} v{} in {}",
            status.service_name, status.version, status.environment
        )))
        .request_id(Some("system_init".to_string()));

    let server_entry = ServerLogEntry::new(Level::INFO, "system_initialization".to_string())
        .with_base(base_entry)
        .user_id(Some("system".to_string()));

    get_global_logger().lock().await.log_server_entry(server_entry).await?;
    Ok(())
}
