fn invalid_name_error(name: &str) -> String {
    format!(
        "invalid app name '{name}': allowed pattern is [A-Za-z0-9._-], cannot start with '-', and cannot be '.' or '..'"
    )
}

fn is_allowed(ch: u8) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, b'.' | b'_' | b'-')
}

pub fn validate_app_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(invalid_name_error(name));
    }

    if name == "." || name == ".." {
        return Err(invalid_name_error(name));
    }

    if name.starts_with('-') {
        return Err(invalid_name_error(name));
    }

    if name.as_bytes().iter().any(|&ch| !is_allowed(ch)) {
        return Err(invalid_name_error(name));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_allows_safe_names() {
        for name in ["app", "my-app", "a1", "app-2", "z9-x8", "My_App", "app.1"] {
            assert!(
                validate_app_name(name).is_ok(),
                "expected valid app name: {name}"
            );
        }
    }

    #[test]
    fn validate_rejects_empty_name() {
        let err = validate_app_name("").unwrap_err();
        assert!(err.contains("invalid app name"));
    }

    #[test]
    fn validate_rejects_invalid_characters() {
        for name in ["app/../x", "../x", "bad name", "bad\tname", "bad:name"] {
            let err = validate_app_name(name).unwrap_err();
            assert!(
                err.contains("allowed pattern is [A-Za-z0-9._-]"),
                "unexpected error for {name}: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_disallowed_prefix_or_segments() {
        for name in ["-app", ".", ".."] {
            assert!(
                validate_app_name(name).is_err(),
                "expected invalid app name: {name}"
            );
        }
    }
}
