# Smartpoll

Smartpoll is a Rust library which provides a `Task` abstraction that simplifies polling futures.

Please note that Smartpoll is still in early development and is not yet thoroughly tested.
It is not currently recommended for use in production environments.

## How it works

Smartpoll's `Task` type wraps around a top-level future. Its `poll` method synchronises calls to
`Future::poll` by communicating with the task's wakers to ensure that the task is not rescheduled
until `Future::poll` has returned. The rescheduling code is a closure passed to `Task::poll`.

See the [examples](examples) for more detailed usage information.

## License

This project is licensed under the [MIT license](LICENSE).