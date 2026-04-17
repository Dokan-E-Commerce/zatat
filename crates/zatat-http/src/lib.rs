#![forbid(unsafe_code)]

pub mod routes;
pub mod sign;

pub use routes::build_api_router;
