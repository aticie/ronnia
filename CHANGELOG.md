# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
- There are no unreleased changes.

## [1.4.3] - 2021-08-13
### Fixed
- Bot now joins the owner's channel if started from scratch

## [1.4.2] - 2021-08-13
### Fixed
- Fields being wrong when requests being added to db

## [1.4.1] - 2021-08-12
### Fixed
- `error_type` variable name mistake

## [1.4.0] - 2021-08-12
### Added
- Statistics Database which keeps information about:
  - API Requests
  - Beatmap Requests & In-Game chat usage
  - General Errors

## [1.3.5] - 2021-08-09
### Fixed
- Channel update routine messing up user information because of unsorted lists.

## [1.3.4] - 2021-08-09
### Fixed
- Minor fixes.

## [1.3.3] - 2021-08-08
### Changed
- User update message being hardcoded to read from a text file.

## [1.3.2] - 2021-08-08
### Fixed
- Test mode being useless for some criteria checks.
- `check_if_streaming_osu()` producing an error.

## [1.3.1] - 2021-08-07
### Fixed
- Twitchio v2 rate-limit being wrong when joining channels.
- Cogs not being added.

## [1.3.0] - 2021-08-07
### Updated
- Twitchio library from v1 to v2.

## [1.2.6] - 2021-07-05
### Added
- Drain Length to requested beatmap information.

## [1.2.5] - 2021-07-05
### Fixed
- Alternate official beatmap link not being parsed correctly.

## [1.2.4] - 2021-06-11
### Fixed
- Multiple of same mods not being omitted when delivering requests.

## [1.2.3] - 2021-06-09
### Fixed
- Edge case for mod text when no mod is given

## [1.2.2] - 2021-06-09
### Added
- Support for different beatmap links and mod combinations. 

## [1.2.1] - 2021-05-27
### Fixed
- Excluded users to be lowercase when comparing and adding to database. 

## [1.2.0] - 2021-05-27
### Added
- Excluded users list so that the bot doesn't respond to unwanted messages. 

## [1.1.0] - 2021-04-25
### Added
- Automated tests with docker builds.
- Unit tests to some twitch bot methods.
- Changelog

[Unreleased]: https://github.com/aticie/ronnia/compare/v1.4.3...HEAD
[1.4.3]: https://github.com/aticie/ronnia/compare/v1.4.2...1.4.3
[1.4.2]: https://github.com/aticie/ronnia/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/aticie/ronnia/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/aticie/ronnia/compare/v1.3.5...v1.4.0
[1.3.5]: https://github.com/aticie/ronnia/compare/v1.3.4...v1.3.5
[1.3.4]: https://github.com/aticie/ronnia/compare/v1.3.3...v1.3.4
[1.3.3]: https://github.com/aticie/ronnia/compare/v1.3.2...v1.3.3
[1.3.2]: https://github.com/aticie/ronnia/compare/v1.3.1...v1.3.2
[1.3.1]: https://github.com/aticie/ronnia/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/aticie/ronnia/compare/v1.2.6...v1.3.0
[1.2.6]: https://github.com/aticie/ronnia/compare/v1.2.5...v1.2.6
[1.2.5]: https://github.com/aticie/ronnia/compare/v1.2.4...v1.2.5
[1.2.4]: https://github.com/aticie/ronnia/compare/v1.2.3...v1.2.4
[1.2.3]: https://github.com/aticie/ronnia/compare/v1.2.2...v1.2.3
[1.2.2]: https://github.com/aticie/ronnia/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/aticie/ronnia/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/aticie/ronnia/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/aticie/ronnia/releases/tag/v1.1.0
