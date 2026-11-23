use crate::asset::AssetId;

/// Errors produced by the pure domain layer.
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum CoreError {
    #[error("universo inválido: {0}")]
    InvalidUniverse(String),
    #[error("serie de precios inválida: {0}")]
    InvalidSeries(String),
    #[error("configuración inválida: {0}")]
    InvalidConfig(String),
    #[error("falta el precio de {0}")]
    MissingPrice(AssetId),
    #[error("datos insuficientes: {0}")]
    MissingData(String),
    #[error("no se pudo interpretar: {0}")]
    Parse(String),
}
