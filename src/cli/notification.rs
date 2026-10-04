use crate::api::schema::{Method, NotificationShowParams, NotificationShowSound, Request};
use crate::config::ToastHerdrPosition;

pub(super) fn run_notification_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_notification_help();
        return Ok(2);
    };

    match subcommand {
        "show" => notification_show(&args[1..]),
        "help" | "--help" | "-h" => {
            print_notification_help();
            Ok(0)
        }
        _ => {
            print_notification_help();
            Ok(2)
        }
    }
}

fn notification_show(args: &[String]) -> std::io::Result<i32> {
    let env_pane_id = super::target::caller_pane_id();
    let params = match parse_notification_show_args(args, env_pane_id.as_deref()) {
        Ok(params) => params,
        Err(NotificationShowArgError::Usage) => {
            eprintln!(
                "usage: herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request|wait] [--pane PANE_ID]"
            );
            return Ok(2);
        }
        Err(NotificationShowArgError::Message(message)) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:notification:show".into(),
        method: Method::NotificationShow(params),
    })?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationShowArgError {
    Usage,
    Message(String),
}

fn parse_notification_show_args(
    args: &[String],
    env_pane_id: Option<&str>,
) -> Result<NotificationShowParams, NotificationShowArgError> {
    let Some(title) = args.first().cloned() else {
        return Err(NotificationShowArgError::Usage);
    };
    if matches!(title.as_str(), "help" | "--help" | "-h") {
        return Err(NotificationShowArgError::Usage);
    }

    let mut body = None;
    let mut position = None;
    let mut sound = NotificationShowSound::None;
    let mut pane_id = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--body" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --body".into(),
                    ));
                };
                body = Some(value.clone());
                index += 2;
            }
            "--position" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --position".into(),
                    ));
                };
                position = Some(parse_toast_position(value)?);
                index += 2;
            }
            "--sound" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --sound".into(),
                    ));
                };
                sound = parse_notification_sound(value)?;
                index += 2;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(NotificationShowArgError::Message(
                        "missing value for --pane".into(),
                    ));
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            other => {
                return Err(NotificationShowArgError::Message(format!(
                    "unknown option: {other}"
                )));
            }
        }
    }

    Ok(NotificationShowParams {
        title,
        body,
        position,
        sound,
        pane_id: pane_id.or_else(|| env_pane_id.map(super::normalize_pane_id)),
    })
}

fn parse_toast_position(value: &str) -> Result<ToastHerdrPosition, NotificationShowArgError> {
    match value {
        "top-left" => Ok(ToastHerdrPosition::TopLeft),
        "top-right" => Ok(ToastHerdrPosition::TopRight),
        "bottom-left" => Ok(ToastHerdrPosition::BottomLeft),
        "bottom-right" => Ok(ToastHerdrPosition::BottomRight),
        _ => Err(NotificationShowArgError::Message(format!(
            "invalid position: {value} (expected top-left, top-right, bottom-left, or bottom-right)"
        ))),
    }
}

fn parse_notification_sound(
    value: &str,
) -> Result<NotificationShowSound, NotificationShowArgError> {
    match value {
        "none" => Ok(NotificationShowSound::None),
        "done" => Ok(NotificationShowSound::Done),
        "request" => Ok(NotificationShowSound::Request),
        "wait" => Ok(NotificationShowSound::Wait),
        _ => Err(NotificationShowArgError::Message(format!(
            "invalid sound: {value} (expected none, done, request, or wait)"
        ))),
    }
}

fn print_notification_help() {
    eprintln!("herdr notification commands:");
    eprintln!(
        "  herdr notification show <title> [--body TEXT] [--position top-left|top-right|bottom-left|bottom-right] [--sound none|done|request|wait] [--pane PANE_ID]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn notification_show_args_parse_title_body_and_position() {
        let params = parse_notification_show_args(
            &args(&[
                "build failed",
                "--body",
                "api workspace",
                "--position",
                "top-right",
                "--sound",
                "request",
            ]),
            None,
        )
        .unwrap();

        assert_eq!(
            params,
            NotificationShowParams {
                title: "build failed".into(),
                body: Some("api workspace".into()),
                position: Some(ToastHerdrPosition::TopRight),
                sound: NotificationShowSound::Request,
                pane_id: None,
            }
        );
    }

    #[test]
    fn notification_show_args_reject_invalid_position() {
        let error = parse_notification_show_args(
            &args(&["build failed", "--position", "top-center"]),
            None,
        )
        .unwrap_err();

        assert_eq!(
            error,
            NotificationShowArgError::Message(
                "invalid position: top-center (expected top-left, top-right, bottom-left, or bottom-right)"
                    .into()
            )
        );
    }

    #[test]
    fn notification_show_args_default_sound_is_none() {
        let params = parse_notification_show_args(&args(&["build failed"]), None).unwrap();

        assert_eq!(params.sound, NotificationShowSound::None);
    }

    #[test]
    fn notification_show_args_reject_invalid_sound() {
        let error = parse_notification_show_args(&args(&["build failed", "--sound", "loud"]), None)
            .unwrap_err();

        assert_eq!(
            error,
            NotificationShowArgError::Message(
                "invalid sound: loud (expected none, done, request, or wait)".into()
            )
        );
    }

    #[test]
    fn notification_show_args_parse_wait_sound() {
        let params = parse_notification_show_args(&args(&["t", "--sound", "wait"]), None).unwrap();

        assert_eq!(params.sound, NotificationShowSound::Wait);
    }

    #[test]
    fn notification_show_args_parse_pane_flag() {
        let params =
            parse_notification_show_args(&args(&["t", "--pane", "w1:p2"]), Some("w1:p9")).unwrap();

        assert_eq!(params.pane_id.as_deref(), Some("w1:p2"));
    }

    #[test]
    fn notification_show_args_default_pane_is_the_caller_pane() {
        let params = parse_notification_show_args(&args(&["t"]), Some("w1:p9")).unwrap();
        assert_eq!(params.pane_id.as_deref(), Some("w1:p9"));

        let params = parse_notification_show_args(&args(&["t"]), None).unwrap();
        assert_eq!(params.pane_id, None);
    }

    #[test]
    fn notification_show_args_reject_missing_pane_value() {
        let error = parse_notification_show_args(&args(&["t", "--pane"]), None).unwrap_err();

        assert_eq!(
            error,
            NotificationShowArgError::Message("missing value for --pane".into())
        );
    }
}
