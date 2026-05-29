use std::{env, ffi::OsString};

use super::ControlError;

#[derive(Debug)]
pub(super) struct ControlConfig {
    pub(super) pid: i32,
    pub(super) throttle: f32,
}

impl ControlConfig {
    pub(super) fn from_env() -> Result<Self, ControlError> {
        let args = parse_env_args(env::args_os())?;
        Self::parse_args(&args)
    }

    fn new(pid: i32, throttle: f32) -> Self {
        Self { pid, throttle }
    }

    fn parse_args(args: &[String]) -> Result<Self, ControlError> {
        if args.len() != 3 {
            return Err(ControlError::InvalidArguments(
                "usage: kirv <pid> <percentage>",
            ));
        }

        let pid: i32 = args[1].trim().parse().map_err(ControlError::InvalidPid)?;
        if pid <= 0 {
            return Err(ControlError::InvalidArguments("pid must be greater than 0"));
        }

        let throttle = args[2]
            .trim()
            .parse()
            .map_err(ControlError::InvalidThrottle)?;
        if !(1.0..=99.0).contains(&throttle) {
            return Err(ControlError::InvalidArguments(
                "percentage must be between 1 and 99",
            ));
        }

        Ok(Self::new(pid, throttle))
    }
}

fn parse_env_args(args: impl IntoIterator<Item = OsString>) -> Result<Vec<String>, ControlError> {
    args.into_iter()
        .enumerate()
        .map(|(index, arg)| {
            if index == 0 {
                Ok(arg.to_string_lossy().into_owned())
            } else {
                arg.into_string()
                    .map_err(|_| ControlError::InvalidArguments("arguments must be valid UTF-8"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parse_env_args_rejects_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;

        let args = vec![
            OsString::from("kirv"),
            OsString::from_vec(vec![0xFF]),
            OsString::from("10"),
        ];

        assert!(matches!(
            parse_env_args(args),
            Err(ControlError::InvalidArguments(
                "arguments must be valid UTF-8"
            ))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn parse_env_args_accepts_non_utf8_program_path() {
        use std::os::unix::ffi::OsStringExt;

        let args = vec![
            OsString::from_vec(vec![b'k', b'i', b'r', b'v', 0xFF]),
            OsString::from("123"),
            OsString::from("10"),
        ];

        let parsed = parse_env_args(args).expect("program path should be converted lossily");

        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1], "123");
        assert_eq!(parsed[2], "10");
    }

    #[test]
    fn parse_args_rejects_invalid_arity() {
        let args = vec!["kirv".to_string(), "123".to_string()];
        assert!(matches!(
            ControlConfig::parse_args(&args),
            Err(ControlError::InvalidArguments(
                "usage: kirv <pid> <percentage>"
            ))
        ));
    }

    #[test]
    fn parse_args_rejects_invalid_pid() {
        let args = vec!["kirv".to_string(), "abc".to_string(), "10".to_string()];
        assert!(matches!(
            ControlConfig::parse_args(&args),
            Err(ControlError::InvalidPid(_))
        ));
    }

    #[test]
    fn parse_args_rejects_non_positive_pid() {
        let args = vec!["kirv".to_string(), "-1".to_string(), "10".to_string()];
        assert!(matches!(
            ControlConfig::parse_args(&args),
            Err(ControlError::InvalidArguments("pid must be greater than 0"))
        ));
    }

    #[test]
    fn parse_args_rejects_out_of_range_percentage() {
        let args = vec!["kirv".to_string(), "123".to_string(), "100".to_string()];
        assert!(matches!(
            ControlConfig::parse_args(&args),
            Err(ControlError::InvalidArguments(
                "percentage must be between 1 and 99"
            ))
        ));
    }

    #[test]
    fn parse_args_accepts_percentage_above_fifty() {
        let args = vec!["kirv".to_string(), "123".to_string(), "99".to_string()];

        let parsed = ControlConfig::parse_args(&args).expect("percentage should be accepted");

        assert_eq!(parsed.throttle, 99.0);
    }

    #[test]
    fn parse_args_rejects_empty_percentage() {
        let args = vec!["kirv".to_string(), "123".to_string(), "".to_string()];
        assert!(matches!(
            ControlConfig::parse_args(&args),
            Err(ControlError::InvalidThrottle(_))
        ));
    }
}
