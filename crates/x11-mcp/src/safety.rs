use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayIdentity {
    host: String,
    display: u16,
    screen: u16,
}

pub fn validate_target_display(
    target: &str,
    inherited: Option<&str>,
    allow_host_display: bool,
) -> Result<()> {
    let target_identity = parse_display(target)?;
    if allow_host_display {
        return Ok(());
    }
    if inherited
        .map(parse_display)
        .transpose()?
        .is_some_and(|inherited| inherited == target_identity)
    {
        bail!(
            "refusing inherited host display {target:?}; use --allow-host-display or an isolated Xvfb/Xephyr display"
        );
    }
    if inherited.is_none() && target_identity.host.is_empty() && target_identity.display == 0 {
        bail!(
            "refusing likely host display {target:?}; use --allow-host-display or an isolated Xvfb/Xephyr display"
        );
    }
    Ok(())
}

fn parse_display(value: &str) -> Result<DisplayIdentity> {
    let value = value.trim();
    let (host, tail) = value.rsplit_once(':').ok_or_else(|| {
        anyhow::anyhow!("invalid X11 display {value:?}; expected HOST:DISPLAY[.SCREEN]")
    })?;
    let host = match host {
        "unix" | "localhost" => "",
        other => other,
    };
    let (display, screen) = tail.split_once('.').unwrap_or((tail, "0"));
    let display = display
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("invalid X11 display number in {value:?}"))?;
    let screen = screen
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("invalid X11 screen number in {value:?}"))?;
    Ok(DisplayIdentity {
        host: host.to_ascii_lowercase(),
        display,
        screen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_local_display_forms() {
        assert_eq!(
            parse_display(":20.0").unwrap(),
            parse_display("unix:20").unwrap()
        );
    }

    #[test]
    fn refuses_inherited_display() {
        assert!(validate_target_display(":0", Some(":0.0"), false).is_err());
        assert!(validate_target_display(":20", Some(":0"), false).is_ok());
        assert!(validate_target_display(":0", Some(":0"), true).is_ok());
    }
}
