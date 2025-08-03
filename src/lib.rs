#[cfg(any(
    all(feature = "work_stealing", feature = "share_nothing"),
    all(feature = "work_stealing", feature = "io_uring_registry"),
    all(feature = "work_stealing", feature = "io_uring_pool"),
    all(feature = "share_nothing", feature = "io_uring_registry"),
    all(feature = "share_nothing", feature = "io_uring_pool"),
    all(feature = "io_uring_registry", feature = "io_uring_pool"),
))]
compile_error!(
    "Only one of the features `work_stealing`, `share_nothing`, `io_uring_registry`, or `io_uring_pool` may be enabled at a time."
);

#[cfg(not(any(
    feature = "work_stealing",
    feature = "share_nothing",
    feature = "io_uring_registry",
    feature = "io_uring_pool"
)))]
compile_error!(
    "You must enable exactly one of the features: `work_stealing`, `share_nothing`, `io_uring_registry`, or `io_uring_pool`."
);

#[macro_use]
extern crate log;

mod http_server;
pub mod io_uring;
mod request;
mod response;

pub use http_server::{HttpServer, HttpService, HttpServiceFactory};
#[cfg(any(feature = "work_stealing", feature = "share_nothing"))]
pub use request::{BodyReader, Request};
pub use response::Response;

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
pub use httparse::Request;
