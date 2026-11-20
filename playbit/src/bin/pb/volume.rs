//! Volume creation chooses key sharing for this volume, without changing other volumes.
use super::*;

pub(super) fn create(args: &[String]) -> Result<Request> {
    let mut name = None;
    let mut share_key_with_service = true;
    for argument in args {
        match argument.as_str() {
            "--no-shared-key" if share_key_with_service => share_key_with_service = false,
            value if !value.starts_with('-') && name.is_none() => name = Some(value.to_owned()),
            _ => return Err("expected pb volume create [--no-shared-key] [LABEL]".into()),
        }
    }
    Ok(Request::CreateVolume {
        name: name.unwrap_or_else(|| "Untitled".into()),
        share_key_with_service,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_shares_keys_unless_this_command_explicitly_opts_out() {
        // Each parse starts with sharing enabled; the opt-out applies to one request.
        for (args, expected_name, shared) in [
            (vec![], "Untitled", true),
            (vec!["New volume"], "New volume", true),
            (vec!["--no-shared-key", "New volume"], "New volume", false),
            (vec!["New volume", "--no-shared-key"], "New volume", false),
            (vec!["--no-shared-key"], "Untitled", false),
            (vec!["Next volume"], "Next volume", true),
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let Request::CreateVolume { name, share_key_with_service } =
                create(&args).expect("valid volume creation arguments")
            else {
                panic!("expected volume creation");
            };
            assert_eq!(name, expected_name);
            assert_eq!(share_key_with_service, shared);
        }
        for args in
            [vec!["--unknown"], vec!["--no-shared-key", "--no-shared-key"], vec!["One", "Two"]]
        {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(create(&args).is_err());
        }
    }
}
