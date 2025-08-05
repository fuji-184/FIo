# FIo

FIo is an experimental HTTP server library. It supports multiple concurrency and I/O strategies, including multithreading with work stealing, isolated per thread runtimes (share nothing), and advanced I/O using io_uring with both reistry and pooled buffer management

## Features

- ⚡ High performance
- 🧵 Multi threaded work stealing mode
- 🔀 Share nothing mode with per-thread runtimes
- 🧵🔧 io_uring support with buffer pool and fixed buffer registry
- 🔒 Optional mode selection: only one runtime mode can be enabled at compile time

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
fio = { git = "https://github.com/fuji-184/FIo.git", features = ["choose the mode you want"] }
```

> Only **one** of the following features may be enabled at a time:
>
> - `work_stealing`
> - `share_nothing`
> - `io_uring_registry`
> - `io_uring_pool`

Enabling more than one will cause a compile time error

## Usage

You must implement the `HttpService` trait for your service. Depending on the active feature, different methods will be called

### 1. Work Stealing

Enable the feature in `Cargo.toml`:

```toml
[dependencies]
fio = { git = "https://github.com/fuji-184/FIo.git", features = ["work_stealing"] }
```

Then:

```rust
use fio::{HttpServer, HttpService, Request, Response};
use std::io;

#[derive(Clone)]
struct Server;

impl HttpService for Server {
    async fn router(&mut self, req: Request<'_, '_, '_>, res: &mut Response<'_>) -> io::Result<()> {
        match req.path() {
            "/" => res.body("hello") // or separated handler function
            _   => res.body("not found")
        }

        Ok(())
    }
}

fn main() {
    HttpServer(Server).start("0.0.0.0:8080");
}
```

### 2. Share Nothing

Enable the feature:

```toml
[dependencies]
fio = { git = "https://github.com/fuji-184/FIo.git", features = ["share_nothing"] }
```

Then:

```rust
use fio::{HttpServer, HttpService, Request, Response};
use std::io;

#[derive(Clone)]
struct Server;

impl HttpService for Server {
    async fn router(&mut self, req: Request<'_, '_, '_>, res: &mut Response<'_>) -> io::Result<()> {
        match req.path() {
            "/" => res.body("hello")
            _   => res.body("not found")
        }

        Ok(())
    }
}

fn main() {
    HttpServer(Server).start("0.0.0.0:8080");
}
```

### 3. io_uring with Registry

Enable the feature:

```toml
[dependencies]
fio = { git = "https://github.com/fuji-184/FIo.git", features = ["io_uring_registry"] }
```

Then:

```rust
use fio::{HttpServer, HttpService, Request, Response};

#[derive(Clone)]
struct Server;

impl HttpService for Server {
    async fn router(&mut self, req: Request<'_, '_>, res: &mut Response<'_>) -> io::Result<()> {
        match req.path.unwrap() {
            "/" => res.body("hello"),
            _   => res.body("not found")
        }

        Ok(())
    }
}

fn main() {
    HttpServer(Server).start("0.0.0.0:8080");
}
```

### 4. io_uring with Buffer Pool

Enable the feature:

```toml
[dependencies]
fio = { git = "https://github.com/fuji-184/FIo.git", features = ["io_uring_pool"] }
```

Then:

```rust
use fio::{HttpServer, HttpService, Request, Response};
use std::io;

#[derive(Clone)]
struct Server;

impl HttpService for Server {
    async fn router(&mut self, req: Request<'_, '_>, res: &mut Response<'_>) -> io::Result<()> {
        match req.path.unwrap() {
            "/" => res.body("hello")
            _   => res::body("not found")
        }
    }

    Ok(())
}

fn main() {
    HttpServer(Server).start("0.0.0.0:8080");
}
```
