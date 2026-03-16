use thiserror::Error;

#[derive(Error, Debug)]
pub enum GithubClawError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Agent spawn error: {0}")]
    AgentSpawn(String),

    #[error("Webhook error: {0}")]
    Webhook(String),

    #[error("Orchestrator error: {0}")]
    Orchestrator(String),

    #[error("Queue error: {0}")]
    Queue(String),

    #[error("Dispatch error: {0}")]
    Dispatch(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Marker parse error: {0}")]
    Marker(String),

    #[error("Pipeline error: {0}")]
    Pipeline(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

pub type Result<T> = std::result::Result<T, GithubClawError>;
