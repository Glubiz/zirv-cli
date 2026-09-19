use std::io::{self, IsTerminal};

/// Topics in the tour, in order of presentation.
const TOPICS: &[&str] = &[
    "overview",
    "harnesses",
    "memory",
    "safety",
    "jev",
    "config",
    "unstuck",
];

/// Section text for each topic.
fn section_text(topic: &str) -> Option<&'static str> {
    match topic {
        "overview" => Some(
            "Overview

zirv is a session supervisor for AI coding. It runs your coding session—Claude Code, \
Codex, or another harness—in a supervised context that:

• Scores your transcript's rot (quality degradation over time)
• Compacts conversations or triggers a handoff before quality drops
• Stores durable facts that survive restarts
• Manages command safety and escalations

Start a session with `zirv ctx chat` or `zirv chat` (interactive) or `zirv ctx agent` \
for headless work.",
        ),
        "harnesses" => Some(
            "Harnesses

A harness is the coding tool zirv drives: Claude Code, Codex, or another AI coding CLI.

See which harnesses are enabled:
  zirv setup status

Pick one for this session:
  zirv ctx config set agent claude
  zirv ctx config set agent codex

zirv refuses to silently switch providers; you choose explicitly.",
        ),
        "memory" => Some(
            "Memory

The memory bank is where facts live that survive sessions. Store them to remember \
decisions, vendor behaviour, standing gotchas, or things that are not in the code.

Store a fact in your repository:
  zirv ctx remember --key mykey --text 'My durable fact'

Store in the machine-local bank:
  zirv ctx remember --key mykey --text 'My durable fact' --global

List facts:
  zirv ctx recall

Remove a fact:
  zirv ctx forget mykey

Memory lives in `.zirv/memory/` (repository) or machine-local state (private).",
        ),
        "safety" => Some(
            "Safety

zirv classifies commands and can ask for approval before risky ones run. \
The policy is set operator-wide or per repository.

Check your safety policy:
  zirv ctx safety list

Explain why a command got its verdict:
  zirv ctx safety explain -- <command>

A repository may only narrow safety, never widen it. \
The operator's setting is the baseline.",
        ),
        "jev" => Some(
            "Jev

Jev is TypeSafe's optional hosted advisor—it is off by default.

Check Jev status:
  zirv ctx jev status

It needs its own API key. See `zirv setup status` for whether you have enabled it. \
Leave disabled unless you have an active subscription.",
        ),
        "config" => Some(
            "Configuration

Your configuration has three layers:

1. Operator config (~/.zirv/ctx.toml): yours, takes precedence
2. Repository config (.zirv/ctx.toml): untrusted, can only narrow settings
3. Environment overrides (ZIRV_CTX_* variables)

View all settings:
  zirv ctx config show

Set a single value:
  zirv ctx config set <key> <value>

Examples:
  zirv ctx config set agent claude
  zirv ctx config set agent codex

The most important section. Operator config at ~/.zirv/ctx.toml is always yours. \
Repository config is read-only for safety.",
        ),
        "unstuck" => Some(
            "When Something Is Wrong

Run one of these to diagnose:

Check sessions and scores:
  zirv ctx status

Check whether setup is complete:
  zirv setup status

List all configured integrations (MCP, web search, etc.):
  zirv ctx capabilities

Diagnose native harness readiness:
  zirv ctx doctor

File a bug:
  zirv report bug --title 'Your title'

Check zirv's full help:
  zirv help",
        ),
        _ => None,
    }
}

/// Run the tour. If topic is None, guide interactively (TTY) or print all (non-TTY).
/// If topic is Some, print that section and exit.
pub fn run(topic: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    if let Some(topic) = topic {
        // Single topic mode: print it and exit
        if let Some(text) = section_text(topic) {
            println!("{}", text);
            Ok(())
        } else {
            let valid_topics = TOPICS.join(", ");
            Err(format!("Unknown topic '{}'. Valid topics: {}", topic, valid_topics).into())
        }
    } else if is_tty {
        // TTY mode: guided paged tour
        run_guided_tour()
    } else {
        // Non-TTY mode: print all sections plainly
        run_noninteractive_tour()
    }
}

/// Interactive paged tour for TTY sessions.
fn run_guided_tour() -> Result<(), Box<dyn std::error::Error>> {
    let mut current = 0;
    let total = TOPICS.len();

    loop {
        let topic = TOPICS[current];
        let text = section_text(topic).unwrap();

        println!("\n{}\n", text);
        println!(
            "{}/{} — [enter] next, [b] back, [q] quit",
            current + 1,
            total
        );

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let choice = input.trim().to_lowercase();

        match choice.as_str() {
            "q" | "quit" => {
                println!(
                    "\nTour paused at '{}'.\nResume with: zirv tour\nJump to a topic with: zirv tour <topic>",
                    topic
                );
                break;
            }
            "b" | "back" => {
                if current > 0 {
                    current -= 1;
                } else {
                    eprintln!("Already at the beginning.");
                }
            }
            "" | "n" | "next" => {
                if current + 1 < total {
                    current += 1;
                } else {
                    println!("\nTour complete. You've seen all {} sections.", total);
                    println!(
                        "Run `zirv tour <topic>` to revisit a section, or `zirv help` for more."
                    );
                    break;
                }
            }
            _ => {
                eprintln!(
                    "Unrecognized input '{}'. Enter: [enter]/n (next), b (back), q (quit)",
                    choice
                );
            }
        }
    }

    Ok(())
}

/// Non-interactive plaintext tour for non-TTY output.
fn run_noninteractive_tour() -> Result<(), Box<dyn std::error::Error>> {
    for (i, topic) in TOPICS.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if let Some(text) = section_text(topic) {
            println!("{}", text);
        }
    }
    Ok(())
}

/// Offer the tour after first-run setup. Prints the offer, reads the answer, and runs
/// the tour on yes. Silent no-op when not a TTY.
#[allow(dead_code)]
pub fn offer_after_first_run() {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return;
    }

    println!("\nWould you like a guided tour of zirv? (y/n)");

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return;
    }

    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => {
            if let Err(e) = run(None) {
                eprintln!("Tour error: {}", e);
            }
        }
        _ => {
            println!("To see the tour later, run: zirv tour");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_topics_have_text() {
        for topic in TOPICS {
            let text = section_text(topic);
            assert!(
                text.is_some() && !text.unwrap().is_empty(),
                "Topic '{}' has no text",
                topic
            );
        }
    }

    #[test]
    fn test_unknown_topic_errors() {
        let result = run(Some("nosuchtopic"));
        assert!(result.is_err(), "Unknown topic should error");
    }

    #[test]
    fn test_known_topics_succeed() {
        for topic in TOPICS {
            let result = run(Some(topic));
            assert!(result.is_ok(), "Topic '{}' should succeed", topic);
        }
    }

    #[test]
    fn test_noninteractive_tour_output() {
        let result = run_noninteractive_tour();
        assert!(result.is_ok(), "Non-interactive tour should not error");
    }

    #[test]
    fn test_offer_after_first_run_is_noop_when_no_tty() {
        // This test verifies the function completes without error when not a TTY.
        // We can't easily test it fails without a TTY in a unit test, but we can
        // ensure it doesn't panic.
        offer_after_first_run(); // Should be a no-op in test environment
    }
}
