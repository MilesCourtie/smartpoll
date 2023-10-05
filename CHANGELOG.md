# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2023-10-05

### Added

- Tasks can now store arbitrary metadata of any type that implements `Send`.
- Tests that check the correctness of the synchronisation algorithm by simulating all possible
  thread schedulings for each test case.
- Explain the implementation, testing method, caveats and edge cases thoroughly in comments.
- An example in the documentation explaining how to implement a basic multithreaded executor.
- This changelog.

### Changed

- Replace initial implementation with faster version that uses atomics.
- `Task::poll` now returns the output of `Future::poll`.
- Rewrite the documentation.
- Update crate metadata to reflect 'no_std' status.

### Removed

- Original usage examples.
- Dependency on `parking_lot` crate.
- Dependency on standard library.

## [0.1.1] - 2023-06-18

### Changed

- Update crate metadata to properly display license name.

## [0.1.0] - 2023-06-18

### Added

- Initial implementation using mutexes.
- Examples demonstrating how to use the crate.
- Basic documentation.
