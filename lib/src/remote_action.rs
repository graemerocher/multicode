use serde::{Deserialize, Serialize};

/// Tool actions that multicode-tui may instruct multicode-remote to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAction {
    /// Open the git review application for a repository path.
    Review,
    /// Open a website (GitHub issue or PR).
    Web,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteActionRequest {
    pub action: RemoteAction,
    pub argument: String,
}

pub fn encode_remote_action_request(request: &RemoteActionRequest) -> String {
    serde_json::to_string(request).expect("remote action request should serialize")
}

pub fn decode_remote_action_request(raw: &str) -> Result<RemoteActionRequest, serde_json::Error> {
    serde_json::from_str(raw)
}

/// How the RemoteActionRequest.argument should be passed to this tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerArgumentMode {
    /// The argument should replace '{}' in the tool command.
    Argument,
    /// The tool should run with the argument as the working directory.
    Chdir,
}

/// Expand the command template for the remote command.
pub fn build_handler_command(
    template: &str,
    argument_mode: HandlerArgumentMode,
    argument: &str,
) -> std::io::Result<(String, Vec<String>)> {
    let placeholder_count = template.matches("{}").count();
    match argument_mode {
        HandlerArgumentMode::Argument if placeholder_count != 1 => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "handler command template must include exactly one '{}' placeholder",
            ));
        }
        HandlerArgumentMode::Chdir if placeholder_count != 0 => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "handler command template must not include '{}' placeholder",
            ));
        }
        _ => {}
    }

    let rendered = match argument_mode {
        HandlerArgumentMode::Argument => template.replace("{}", argument),
        HandlerArgumentMode::Chdir => template.to_string(),
    };
    let args = shell_words::split(&rendered).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("handler command could not be parsed: {err}"),
        )
    })?;
    let Some(program) = args.first().cloned() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "handler command must contain a program",
        ));
    };
    Ok((program, args.into_iter().skip(1).collect()))
}
