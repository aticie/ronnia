use super::*;

#[test]
fn origins_are_serialized_before_deriving_defaults() {
    for (input, expected) in [
        ("HTTPS://API.EXAMPLE:443/", "https://api.example"),
        ("HTTP://LOCALHOST:80/", "http://localhost"),
        ("https://api.example:8443/", "https://api.example:8443"),
        ("http://[::1]:8080/", "http://[::1]:8080"),
    ] {
        let normalized = origin("API_ORIGIN", input).unwrap();
        assert_eq!(normalized, expected);
        assert_eq!(origin("PUBLIC_ORIGIN", input).unwrap(), expected);
    }
}

#[test]
fn origins_reject_non_origin_urls() {
    for input in [
        "",
        "example.com",
        "ftp://api.example/",
        "https://api.example/path",
        "https://api.example/?query",
        "https://api.example/#fragment",
        "https://user@api.example/",
        "https://user:password@api.example/",
    ] {
        assert!(origin("API_ORIGIN", input).is_err(), "{input}");
    }
}

#[test]
fn database_paths_reject_empty_and_whitespace_values() {
    for input in ["", " ", "\t\r\n", "\u{2003}"] {
        let error = validate_database_path(Path::new(input)).unwrap_err();
        assert!(error.to_string().contains("DATABASE_PATH"), "{input:?}");
    }
    for input in [
        "ronnia.sqlite3",
        "/data/ronnia.sqlite3",
        " data file.sqlite3 ",
    ] {
        assert!(
            validate_database_path(Path::new(input)).is_ok(),
            "{input:?}"
        );
    }
}
