use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum IolError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IOL respondió {status}: {message}")]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
    },
    #[error("autenticación IOL: {0}")]
    Auth(String),
    #[error("respuesta inválida de IOL: {0}")]
    Parse(String),
}

/// IOL login credentials. `Debug` never prints the password.
#[derive(Clone)]
pub struct IolCredentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for IolCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IolCredentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}
