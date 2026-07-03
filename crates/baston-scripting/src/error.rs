//! Error type for the scripting crate.

/// Errors produced by the scripting runtime and host.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("failed to initialize script runtime for resource {resource}: {message}")]
    RuntimeInit { resource: String, message: String },

    #[error("script execution failed in resource {resource} ({script}): {message}")]
    Execute {
        resource: String,
        script: String,
        message: String,
    },

    #[error("script host thread is no longer running")]
    HostGone,

    #[error("failed to start script host: {0}")]
    HostStart(String),

    #[error("resource {0} is not loaded")]
    ResourceNotLoaded(String),
}
