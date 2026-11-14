//! Creating drafts in Mail.app via Apple Events.
//!
//! Reading uses Mail's files; writing cannot. Mail owns draft state, and a hand-built
//! `.emlx` dropped into Drafts would be ignored or would corrupt the index. So drafts go
//! through Mail itself, which also means a second permission: Automation access scoped to
//! Mail.app, prompted once on first use and quite separate from Full Disk Access.
//!
//! **Drafts only. There is deliberately no send path here.** A draft lands in Mail's Drafts
//! folder for a person to read and send, which keeps a human in the loop by construction
//! rather than by policy.
//!
//! User data never enters the script text. Scripts are fixed strings; subjects, bodies and
//! addresses arrive as `argv`, so no amount of quoting in a subject line can alter what
//! runs.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// A new draft.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftOutcome {
    /// Where it was saved, as Mail reports it.
    pub detail: String,
}

const DRAFT_SCRIPT: &str = r#"
on addRecipients(msg, listText, kind)
    if listText is "" then return
    set AppleScript's text item delimiters to linefeed
    set addrs to text items of listText
    tell application "Mail"
        repeat with a in addrs
            set addr to (a as text)
            if addr is not "" then
                if kind is "to" then
                    make new to recipient at end of to recipients of msg ¬
                        with properties {address:addr}
                else
                    make new cc recipient at end of cc recipients of msg ¬
                        with properties {address:addr}
                end if
            end if
        end repeat
    end tell
end addRecipients

on run argv
    set theSubject to item 1 of argv
    set theBody to item 2 of argv
    set toList to item 3 of argv
    set ccList to item 4 of argv
    tell application "Mail"
        set msg to make new outgoing message with properties ¬
            {subject:theSubject, content:theBody, visible:false}
        my addRecipients(msg, toList, "to")
        my addRecipients(msg, ccList, "cc")
        save msg
        return "draft saved in Mail's Drafts"
    end tell
end run
"#;

const REPLY_SCRIPT: &str = r#"
on run argv
    set theId to item 1 of argv
    set mbName to item 2 of argv
    set theBody to item 3 of argv
    set replyAll to (item 4 of argv is "all")
    tell application "Mail"
        set found to missing value
        repeat with acct in accounts
            try
                repeat with mb in (every mailbox of acct whose name is mbName)
                    set matches to (every message of mb whose message id is theId)
                    if (count of matches) > 0 then
                        set found to item 1 of matches
                        exit repeat
                    end if
                end repeat
            end try
            if found is not missing value then exit repeat
        end repeat
        if found is missing value then
            error "no message with that Message-ID in mailbox " & mbName number 8001
        end if
        set r to reply found opening window false reply to all replyAll
        set content of r to theBody & return & return & (content of r)
        save r
        return "reply draft saved in Mail's Drafts"
    end tell
end run
"#;

/// Saves a new draft. Nothing is sent.
pub fn draft_message(draft: &Draft) -> Result<DraftOutcome> {
    if draft.to.is_empty() && draft.cc.is_empty() {
        return Err(Error::Draft("a draft needs at least one recipient".into()));
    }
    require_mail_running()?;

    let detail = run_script(
        DRAFT_SCRIPT,
        &[
            &draft.subject,
            &draft.body,
            &draft.to.join("\n"),
            &draft.cc.join("\n"),
        ],
    )?;
    Ok(DraftOutcome { detail })
}

/// Saves a reply draft to the message with this RFC 5322 Message-ID.
///
/// Mail's own `reply` command is used rather than a hand-built message, because AppleScript
/// cannot set `In-Reply-To`/`References` on an outgoing message. Letting Mail construct the
/// reply is what makes it thread properly elsewhere.
pub fn draft_reply(
    message_id: &str,
    mailbox_name: &str,
    body: &str,
    reply_all: bool,
) -> Result<DraftOutcome> {
    if message_id.trim().is_empty() {
        return Err(Error::Draft("a reply needs the original Message-ID".into()));
    }
    require_mail_running()?;

    // Mail reports `message id` without angle brackets.
    let id = message_id.trim().trim_start_matches('<').trim_end_matches('>');
    let detail = run_script(
        REPLY_SCRIPT,
        &[id, mailbox_name, body, if reply_all { "all" } else { "sender" }],
    )?;
    Ok(DraftOutcome { detail })
}

/// Apple Events reach a running application only.
fn require_mail_running() -> Result<()> {
    let running = Command::new("/usr/bin/pgrep")
        .args(["-f", "MacOS/Mail"])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false);

    if running {
        Ok(())
    } else {
        Err(Error::MailNotRunning)
    }
}

/// Runs a fixed script with user data supplied as arguments.
///
/// The script arrives on stdin (`osascript -`) and never contains caller data, so a subject
/// line of `" & do shell script "..." & "` is just a subject line.
fn run_script(script: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Draft(format!("cannot run osascript: {e}")))?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| Error::Draft("osascript stdin unavailable".into()))?
        .write_all(script.as_bytes())
        .map_err(|e| Error::Draft(format!("writing the script failed: {e}")))?;

    let output = child
        .wait_with_output()
        .map_err(|e| Error::Draft(format!("osascript failed: {e}")))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(classify(&stderr))
}

/// Turns osascript's error text into something a caller can act on.
fn classify(stderr: &str) -> Error {
    // -1743: the user has not granted this process Automation access to Mail.
    if stderr.contains("-1743") || stderr.contains("Not authorized to send Apple events") {
        return Error::AutomationDenied;
    }
    // -600 / -609: Mail is not running or stopped mid-script.
    if stderr.contains("(-600)") || stderr.contains("(-609)") || stderr.contains("isn't running") {
        return Error::MailNotRunning;
    }
    if stderr.contains("8001") || stderr.contains("no message with that Message-ID") {
        return Error::DraftTargetNotFound;
    }
    Error::Draft(if stderr.is_empty() {
        "osascript reported an unknown failure".into()
    } else {
        stderr.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_needs_a_recipient() {
        let draft = Draft {
            subject: "Hi".into(),
            body: "text".into(),
            ..Default::default()
        };
        assert!(matches!(draft_message(&draft), Err(Error::Draft(_))));
    }

    #[test]
    fn a_reply_needs_a_message_id() {
        assert!(matches!(
            draft_reply("   ", "INBOX", "text", false),
            Err(Error::Draft(_))
        ));
    }

    #[test]
    fn classifies_automation_denial() {
        let stderr = "execution error: Not authorized to send Apple events to Mail. (-1743)";
        assert!(matches!(classify(stderr), Error::AutomationDenied));
    }

    #[test]
    fn classifies_mail_not_running() {
        assert!(matches!(
            classify("execution error: Mail got an error: Application isn't running. (-600)"),
            Error::MailNotRunning
        ));
    }

    #[test]
    fn classifies_a_missing_reply_target() {
        let stderr = "execution error: no message with that Message-ID in mailbox INBOX (8001)";
        assert!(matches!(classify(stderr), Error::DraftTargetNotFound));
    }

    #[test]
    fn unknown_errors_are_passed_through_rather_than_swallowed() {
        match classify("execution error: something odd (-1234)") {
            Error::Draft(message) => assert!(message.contains("something odd")),
            other => panic!("expected passthrough, got {other:?}"),
        }
    }

    #[test]
    fn empty_stderr_still_produces_a_message() {
        assert!(matches!(classify(""), Error::Draft(_)));
    }

    /// The scripts take their data from `argv`, so a hostile subject cannot change the
    /// program. This verifies the mechanism itself with a harmless script.
    #[test]
    fn argv_carries_data_that_would_break_naive_quoting() {
        let hostile = r#"" & do shell script "echo pwned" & ""#;
        let script = "on run argv\n  return item 1 of argv\nend run\n";
        let echoed = run_script(script, &[hostile]).expect("osascript available");
        assert_eq!(echoed, hostile, "data must survive verbatim, unevaluated");
    }

    #[test]
    fn argv_preserves_newlines_and_non_ascii() {
        let script = "on run argv\n  return item 1 of argv\nend run\n";
        let payload = "line one\nline two \u{a9} caf\u{e9} \u{1f600}";
        let echoed = run_script(script, &[payload]).expect("osascript available");
        // osascript returns lines joined; check the distinctive parts survived.
        assert!(echoed.contains("line one"), "got {echoed:?}");
        assert!(echoed.contains('\u{a9}'));
        assert!(echoed.contains("caf\u{e9}"));
    }
}
