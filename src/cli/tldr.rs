pub const QUICK_REFERENCE: &str = r#"QUICK REFERENCE

  Set identity

      export RITE_AGENT=$(rite generate-name)
      rite --agent my-agent whoami
      rite whoami

  Send messages

      rite send general "Starting work on the parser"
      rite send #general "Status: tests are passing"
      rite send @other-agent "Can you review the API shape?"

  Read messages

      rite history general -n 20
      rite history @other-agent -n 20
      rite history general --from other-agent
      rite history general --format json

  Check unread messages

      rite inbox
      rite inbox --all
      rite inbox -c general --mark-read
      rite inbox --mentions
      rite inbox --count-only

  Wait for responses

      rite wait -c general -t 300
      rite wait -c general --from other-agent -t 300
      rite wait --mentions -t 300
      rite history @other-agent -f

  Answer a specific message (threads)

      rite send general "Review 42 ready" --format json    # prints the new id
      rite send general "on it" --reply-to <message-id>
      rite send "$RITE_CHANNEL" "on it" --reply-to "$RITE_MESSAGE_ID"
      rite history --thread <message-id>

  Stream mentions and DMs (one process, all channels, JSONL)

      rite mentions follow --format json
      rite mentions follow --format json --no-dms

  Continue a conversation

      rite history rite -n 20
      rite send rite "@other-agent What do you think about this approach?"
      rite wait -c rite --from other-agent -t 300
      rite send rite "@other-agent Thanks, I'll adjust that."

  Mark messages read

      rite mark-read general
      rite inbox -c general --mark-read

  Find and inspect

      rite search "migration plan"
      rite messages get <message-id>
      rite channels list
      rite agents --active

CONVENTIONS

  Channels are lowercase names like general, backend, or project-topic.
  You may write channels as general or #general.
  Direct messages use @agent-name.
  Use --agent <name> for one command, or RITE_AGENT for a shell/session.
  Use --format text for compact agent-readable output.
  Use --format json when another tool will parse the result.

NEXT HELP

      rite --help
      rite <command> --help
"#;

pub fn run() {
    print!("{QUICK_REFERENCE}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_reference_covers_core_agent_workflows() {
        for expected in [
            "export RITE_AGENT=$(rite generate-name)",
            "rite send general",
            "rite send @other-agent",
            "rite history general",
            "rite inbox --mentions",
            "rite wait --mentions",
            "rite wait -c general --from other-agent",
            "Continue a conversation",
            "rite mark-read general",
            "--format json",
            "--reply-to",
            "rite history --thread",
        ] {
            assert!(
                QUICK_REFERENCE.contains(expected),
                "missing quick reference example: {expected}"
            );
        }
    }

    #[test]
    fn quick_reference_stays_out_of_advanced_workflows() {
        for advanced in ["hooks", "claims", "sync", "telegram"] {
            assert!(
                !QUICK_REFERENCE.to_lowercase().contains(advanced),
                "quick reference should not include advanced topic: {advanced}"
            );
        }
    }
}
