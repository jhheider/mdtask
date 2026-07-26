use crate::model::{Arg, Job, KNOWN_FILE_OPTS, KNOWN_OPTS, Requirement, TaskFile};
use crate::run::is_known_lang;

/// Parse a markdown task file. It is line-based (no CommonMark dependency): a
/// heading starts a job, the first fenced block under it is the script, and
/// `Key: value` lines set metadata. Parsing is infallible; problems are reported
/// in [`TaskFile::warnings`] rather than dropped to silence. CRLF endings are
/// normalized.
pub fn parse(src: &str) -> TaskFile {
    let mut file = TaskFile::default();
    let mut cur: Option<Job> = None;
    let mut in_fence = false;
    let mut fence_marker = "";
    let mut have_script = false; // first fence per job only
    let mut script = String::new();
    // Whether a `Key: value` line here is metadata or just a sentence that
    // happens to start that way. See `apply_line`.
    let mut block_start = true;

    for raw in src.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw); // normalize CRLF
        if in_fence {
            // A fence is closed only by a BARE marker line (CommonMark): ` ``` `
            // with an info string opens, it does not close, so a stray fence-open
            // cannot accidentally terminate an unterminated block early.
            if is_closing_fence(line, fence_marker) {
                in_fence = false;
                block_start = true; // a fence is a block boundary
                if let Some(t) = cur.as_mut()
                    && !have_script
                {
                    t.script = std::mem::take(&mut script);
                    have_script = true;
                }
                script.clear();
            } else if cur.is_some() && !have_script {
                script.push_str(line);
                script.push('\n');
            }
            continue;
        }
        if let Some(marker) = opening_fence(line) {
            in_fence = true;
            block_start = true;
            fence_marker = marker;
            if let Some(t) = cur.as_mut()
                && !have_script
            {
                t.lang = info_string(line, marker);
            }
            script.clear();
            continue;
        }
        if let Some(name) = heading(line) {
            finalize(cur.take(), &mut file);
            cur = Some(Job {
                name,
                ..Job::default()
            });
            have_script = false;
            block_start = true;
            continue;
        }
        if line.trim().is_empty() {
            block_start = true;
            // Keep the paragraph break. Descriptions used to be a run of lines
            // with every blank dropped, so nothing downstream could tell where
            // the opening thought ended: a listing had no first paragraph to
            // show, only a first hard-wrapped line, which is a fragment.
            if let Some(t) = cur.as_mut()
                && !t.description.is_empty()
                && !t.description.ends_with("\n\n")
            {
                t.description.push('\n');
            }
            continue;
        }
        let was_meta = apply_line(
            line,
            cur.as_mut(),
            &mut file.env,
            &mut file.opts,
            &mut file.warnings,
            block_start,
        );
        // A metadata line does not end the run, so `Args:` and `Requires:` can
        // sit together. Nor does a list item, which opens a block of its own and
        // is how an indented `Env:` under a bullet stays reachable. An ordinary
        // sentence does end it: what follows a sentence is its continuation.
        block_start = was_meta || opens_list_item(line);
    }
    // An unterminated fence at EOF: still capture the script so the job is not
    // lost, but warn, since a forgotten closing fence is a common authoring slip.
    if in_fence {
        if let Some(t) = cur.as_mut()
            && !have_script
        {
            t.script = std::mem::take(&mut script);
        }
        let name = cur.as_ref().map(|t| t.name.clone()).unwrap_or_default();
        file.warnings
            .push(format!("unterminated code fence in task {name:?}"));
    }
    finalize(cur.take(), &mut file);
    file
}

/// Finalize a heading into the file. A heading with a script is a job; one without
/// (a `# Tasks` section) is not, but its `Env:` hoists to all jobs. Records
/// warnings for a duplicate name or an unknown fence language.
fn finalize(job: Option<Job>, file: &mut TaskFile) {
    let Some(mut t) = job else {
        return;
    };
    if t.script.is_empty() {
        file.env.append(&mut t.env); // section heading, so hoist its env
        return;
    }
    t.description = t.description.trim().to_string();
    if file.jobs.iter().any(|x| x.name == t.name) {
        file.warnings.push(format!(
            "duplicate task {:?}; the first defined wins",
            t.name
        ));
    }
    if !is_known_lang(&t.lang) {
        file.warnings.push(format!(
            "task {:?}: fenced language {:?} is not a known interpreter; \
             running as a strict sh script",
            t.name, t.lang
        ));
    }
    file.jobs.push(t);
}

/// The opening fence marker if `line` starts one, else `None`.
fn opening_fence(line: &str) -> Option<&'static str> {
    let t = line.trim_start();
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// Whether `line` is a bare closing fence for `marker`: only the fence char, no
/// info string, per CommonMark's closing rule.
fn is_closing_fence(line: &str, marker: &str) -> bool {
    let ch = marker.as_bytes()[0];
    let t = line.trim();
    t.len() >= 3 && t.bytes().all(|b| b == ch)
}

/// Whether `line` opens a markdown list item (`-`, `*`, `+`, or `1.` / `1)`).
///
/// Such a line begins a block, so metadata indented beneath it is still
/// metadata. Without this, documenting a task as a bulleted list and hanging an
/// `Env:` off one of the bullets would quietly stop working.
fn opens_list_item(line: &str) -> bool {
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
        return rest.starts_with(' ') || rest.is_empty();
    }
    let digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
    if digits.len() < t.len()
        && let Some(rest) = digits.strip_prefix(['.', ')'])
    {
        return rest.starts_with(' ') || rest.is_empty();
    }
    false
}

/// The info-string language after the opening fence marker.
fn info_string(line: &str, marker: &str) -> String {
    line.trim_start()
        .strip_prefix(marker)
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

/// The heading text if `line` is an ATX heading (`#`..`######`), else `None`.
fn heading(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let after = t.trim_start_matches('#');
    // Must have a space after the `#` run (a real ATX heading), and not be all #.
    if after == t || !after.starts_with(' ') {
        return None;
    }
    Some(after.trim().to_string())
}

/// Apply a body line: a recognized `Key: value` sets metadata (case-insensitive
/// key, xc vocabulary); anything else is description. `Env:` before the first job
/// Parse a `Requires:` value into requirements.
///
/// Comma-separated. A bare entry is a name with no arguments; an entry wrapped
/// in parentheses is a name followed by whitespace-separated arguments, which is
/// just's `(dist module)` shape. Splitting on commas first is what makes the
/// parenthesised form unambiguous: whitespace can mean "next argument" precisely
/// because it never had to mean "next dependency".
/// Split a `Requires:` value into its entries, on commas that are not inside a
/// parenthesised entry. `(deploy a, b)` is one entry with two arguments, not two
/// entries, which is why this is not `value.split(',')`.
fn split_entries(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&value[start..]);
    out
}

/// Split the inside of a parenthesised entry into a name and its arguments.
///
/// Whitespace or a comma separates. A comma already means "next thing" at the
/// entry level, so `(deploy a, b)` reading as two arguments is what anyone will
/// expect; a literal comma needs quoting.
///
/// Two things are held together across a separator: a `{{ ... }}` placeholder
/// (one token however it is spaced, so `{{ module }}` stays a placeholder rather
/// than becoming three arguments), and a double-quoted run (so an argument may
/// contain a space or a comma at all).
fn split_args(inner: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut rest = inner;

    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() || c == ',' {
            if has {
                out.push(std::mem::take(&mut cur));
                has = false;
            }
            rest = &rest[c.len_utf8()..];
        } else if rest.starts_with("{{") {
            // Verbatim through the closing braces, so `substitute` sees an
            // intact placeholder later. Unterminated, it is just literal text.
            let end = rest.find("}}").map_or(rest.len(), |i| i + 2);
            cur.push_str(&rest[..end]);
            has = true;
            rest = &rest[end..];
        } else if c == '"' {
            let body = &rest[1..];
            let end = body.find('"');
            cur.push_str(end.map_or(body, |i| &body[..i]));
            has = true;
            rest = end.map_or("", |i| &body[i + 1..]);
        } else {
            cur.push(c);
            has = true;
            rest = &rest[c.len_utf8()..];
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn parse_requires(value: &str) -> Vec<Requirement> {
    split_entries(value)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            match entry
                .strip_prefix('(')
                .and_then(|rest| rest.strip_suffix(')'))
            {
                Some(inner) => {
                    let mut parts = split_args(inner).into_iter();
                    Requirement {
                        name: parts.next().unwrap_or_default(),
                        args: parts.collect(),
                    }
                }
                None => Requirement {
                    name: entry.to_string(),
                    args: Vec::new(),
                },
            }
        })
        .filter(|r: &Requirement| !r.name.is_empty())
        .collect()
}

/// The metadata keys, and the aliases each accepts.
const KEYS: &[&str] = &[
    "env",
    "environment",
    "opts",
    "options",
    "args",
    "arguments",
    "requires",
    "req",
    "agent",
];

/// The key `word` was probably meant to be, if it is close enough to one.
///
/// Deliberately narrow: a singular/plural slip or one wrong character. Anything
/// looser would start warning about ordinary prose, which is the thing this
/// format has to live alongside.
fn nearest_key(word: &str) -> Option<&'static str> {
    KEYS.iter()
        .copied()
        .find(|k| {
            // "arg" vs "args", "require" vs "requires", "opt" vs "opts".
            k.strip_suffix('s') == Some(word)
                || word.strip_suffix('s') == Some(*k)
                || k.starts_with(word) && k.len() == word.len() + 1
        })
        .or_else(|| KEYS.iter().copied().find(|k| edit_distance_one(k, word)))
}

/// Whether two words differ by exactly one substitution. Cheap, and enough for
/// the typos that actually happen in a metadata key.
fn edit_distance_one(a: &str, b: &str) -> bool {
    if a.len() != b.len() || a == b {
        return false;
    }
    a.bytes().zip(b.bytes()).filter(|(x, y)| x != y).count() == 1
}

/// accumulates into the hoisted `file_env`.
///
/// `block_start` is whether this line begins a block: it follows the heading, a
/// blank line, a fence, or another metadata line. Metadata is only recognized
/// there, because otherwise a wrapped sentence decides the task's configuration.
/// This was reachable:
///
/// ```markdown
/// The reviewer decides whether to set
/// Agent: allow
/// on a task, which should be rare.
/// ```
///
/// which read as prose to every human and as *opt this task in to agent
/// execution* to the parser. Requiring a block boundary costs nothing, because
/// every real task file already writes its metadata on its own lines, and it
/// makes the security-relevant key unreachable from inside a paragraph.
///
/// Returns whether the line was consumed as metadata, which is how the caller
/// keeps a run of `Args:` / `Requires:` lines together.
fn apply_line(
    line: &str,
    job: Option<&mut Job>,
    file_env: &mut Vec<(String, String)>,
    file_opts: &mut Vec<String>,
    warnings: &mut Vec<String>,
    block_start: bool,
) -> bool {
    if let Some((key, value)) = split_key(line) {
        // A recognized key mid-paragraph is prose. Say so rather than silently
        // dropping it: if it really was meant as metadata, the author needs to
        // know it did nothing.
        if !block_start && KEYS.contains(&key.as_str()) {
            warnings.push(format!(
                "`{key}:` is inside a paragraph, so it was read as description, \
                 not metadata; put it on its own line after a blank one"
            ));
            describe(line, job);
            return false;
        }
        let value = value.trim();
        match key.as_str() {
            "env" | "environment" => {
                let pairs = parse_env(value);
                match job {
                    Some(t) => t.env.extend(pairs),
                    None => file_env.extend(pairs), // hoisted
                }
                return true;
            }
            "opts" | "options" => {
                let Some(t) = job else {
                    // Before the first heading: a file-level option, which is a
                    // different vocabulary from a task's.
                    for flag in value.split_whitespace() {
                        if KNOWN_FILE_OPTS.contains(&flag) {
                            file_opts.push(flag.to_string());
                        } else {
                            warnings.push(format!(
                                "unknown file-level option {flag:?} in `Opts:` (known: {}); \
                                 a task option belongs under a task heading",
                                KNOWN_FILE_OPTS.join(", ")
                            ));
                        }
                    }
                    return true;
                };
                // Extend, not assign. Assigning meant a second `Opts:` line
                // silently erased the first, and a second `Requires:` line
                // silently dropped a dependency while still exiting 0.
                t.opts.extend(value.split_whitespace().map(str::to_string));
                for flag in &t.opts {
                    if !KNOWN_OPTS.contains(&flag.as_str()) {
                        let hint = if KNOWN_FILE_OPTS.contains(&flag.as_str()) {
                            "; that one is file-level, so it goes before the first task heading"
                        } else {
                            ""
                        };
                        warnings.push(format!(
                            "unknown option {flag:?} in `Opts:` (known: {}){hint}",
                            KNOWN_OPTS.join(", ")
                        ));
                    }
                }
                return true;
            }
            "args" | "arguments" => {
                if let Some(t) = job {
                    t.args.extend(parse_args(value));
                }
                return true;
            }
            "requires" | "req" => {
                if let Some(t) = job {
                    t.requires.extend(parse_requires(value));
                }
                return true;
            }
            "agent" => {
                if let Some(t) = job {
                    t.agent_allow = value.eq_ignore_ascii_case("allow");
                }
                return true;
            }
            other => {
                // A near-miss of a real key is a typo, not prose. `Arg:`,
                // `Require:` and `Opt:` all used to vanish into the description
                // with no warning, so a declared dependency never ran and a
                // declared argument never existed, silently and with exit 0.
                //
                // Only near-misses warn. An ordinary sentence starting "Note:"
                // must stay description, or the format cannot coexist with the
                // prose it is written in.
                if let Some(meant) = nearest_key(other) {
                    warnings.push(format!(
                        "unknown metadata key {other:?}; did you mean {meant:?}? \
                         (treating the line as description)"
                    ));
                }
            }
        }
    }
    describe(line, job);
    false
}

/// Append a line to the job's description. Stray prose outside a job is dropped.
fn describe(line: &str, job: Option<&mut Job>) {
    if let Some(t) = job
        && !line.trim().is_empty()
    {
        t.description.push_str(line.trim());
        t.description.push('\n');
    }
}

/// Split `Key: value`, returning the lowercased key if the line looks like one
/// (a single-word key before the first colon). Leading indentation is allowed, so
/// an `Env:` indented under a list still counts. This is safe because only *known*
/// keys act (see `apply_line`), so ordinary prose with a colon stays description.
fn split_key(line: &str) -> Option<(String, &str)> {
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some((key.to_ascii_lowercase(), &line[colon + 1..]))
}

/// Parse an `Env:` value: comma-separated `KEY=VALUE` pairs.
fn parse_env(value: &str) -> Vec<(String, String)> {
    value
        .split(',')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Parse an `Args:` value into declared [`Arg`]s (just's syntax): `name` is
/// required, `*name` collects the rest (variadic), `name='default'` (or
/// `name="default"`) is optional. Tokens are whitespace-separated, but a quoted
/// default may itself contain spaces (`msg='hello world'`).
fn parse_args(value: &str) -> Vec<Arg> {
    tokenize_args(value)
        .into_iter()
        .filter_map(|tok| {
            let (name, default) = match tok.split_once('=') {
                Some((n, d)) => (n, Some(unquote(d).to_string())),
                None => (tok.as_str(), None),
            };
            let (name, variadic) = match name.strip_prefix('*') {
                Some(rest) => (rest, true),
                None => (name, false),
            };
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(Arg {
                name: name.to_string(),
                variadic,
                default,
            })
        })
        .collect()
}

/// Split an `Args:` value on whitespace, but keep a single- or double-quoted run
/// (a default value) together so `msg='a b'` is one token.
fn tokenize_args(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in value.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c == '\'' || c == '"' => {
                cur.push(c);
                quote = Some(c);
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Strip one matching pair of surrounding single or double quotes, if present.
fn unquote(s: &str) -> &str {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_names(reqs: &[Requirement]) -> Vec<&str> {
        reqs.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn parses_named_jobs_with_interpreter() {
        let tf =
            parse("## build\n\n```sh\ncargo build\n```\n\n## check\n\n```zsh\nprint hi\n```\n");
        assert_eq!(tf.jobs.len(), 2);
        assert_eq!(tf.jobs[0].name, "build");
        assert_eq!(tf.jobs[0].lang, "sh");
        assert_eq!(tf.jobs[0].script.trim(), "cargo build");
        assert_eq!(tf.jobs[1].lang, "zsh");
    }

    #[test]
    fn metadata_keys_are_case_insensitive() {
        let tf = parse(
            "## deploy\n\nOPTS: inherit-cwd\nEnv: REGION=us, TIER=prod\nArgs: target\nRequires: build, test\nAgent: allow\n\n```sh\necho go\n```\n",
        );
        let t = &tf.jobs[0];
        assert_eq!(t.opts, vec!["inherit-cwd"]);
        assert!(t.inherits_cwd());
        assert_eq!(
            t.env,
            vec![
                ("REGION".into(), "us".into()),
                ("TIER".into(), "prod".into())
            ]
        );
        assert_eq!(
            t.args.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["target"]
        );
        assert_eq!(req_names(&t.requires), ["build", "test"]);
        assert!(t.agent_allow);
    }

    #[test]
    fn agent_gate_is_off_by_default() {
        let tf = parse("## secret\n\n```sh\nrm -rf /\n```\n");
        assert!(!tf.jobs[0].agent_allow);
    }

    #[test]
    fn top_level_env_is_hoisted() {
        let tf = parse("# Tasks\n\nEnv: SHARED=1\n\n## a\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.env, vec![("SHARED".into(), "1".into())]);
    }

    #[test]
    fn fence_content_is_not_parsed_as_structure() {
        // A `## heading` and a `Key:` line inside a fence stay in the script.
        let tf = parse("## a\n\n```sh\n## not a task\nEnv: NOPE=1\n```\n");
        assert_eq!(tf.jobs.len(), 1);
        assert!(tf.jobs[0].script.contains("## not a task"));
        assert!(tf.jobs[0].env.is_empty());
    }

    #[test]
    fn an_unknown_opt_warns_but_is_ignored() {
        let tf = parse("## t\n\nOpts: inherit-cwd bogus\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.jobs[0].opts, vec!["inherit-cwd", "bogus"]);
        assert!(tf.jobs[0].inherits_cwd()); // the known flag still applies
        assert!(tf.warnings.iter().any(|w| w.contains("bogus")));
    }

    // ---- parameterized dependencies ---------------------------------------

    #[test]
    fn a_bare_requirement_carries_no_arguments() {
        let reqs = parse_requires("build, test");
        assert_eq!(req_names(&reqs), ["build", "test"]);
        assert!(reqs.iter().all(|r| r.args.is_empty()));
    }

    #[test]
    fn a_parenthesised_requirement_carries_its_arguments() {
        let reqs = parse_requires("lint, (dist bonus-die)");
        assert_eq!(req_names(&reqs), ["lint", "dist"]);
        assert!(reqs[0].args.is_empty(), "the bare one is untouched");
        assert_eq!(reqs[1].args, ["bonus-die"]);
    }

    /// `{{ module }}` is conventionally written with spaces, and a plain
    /// whitespace split turns it into three arguments. It must survive whole or
    /// the syntax is unusable in the form everyone will write it in.
    #[test]
    fn a_spaced_placeholder_stays_one_argument() {
        assert_eq!(
            parse_requires("(dist {{ module }})")[0].args,
            ["{{ module }}"]
        );
        assert_eq!(parse_requires("(dist {{module}})")[0].args, ["{{module}}"]);
        // And a placeholder is a token, not the whole argument.
        assert_eq!(
            parse_requires("(dist {{ module }}-docs)")[0].args,
            ["{{ module }}-docs"]
        );
    }

    /// Entries are comma-separated and arguments are space-separated, so a comma
    /// inside the parentheses would otherwise cut an entry in half and leave a
    /// requirement named `b)`.
    #[test]
    fn a_comma_inside_parentheses_does_not_split_the_entry() {
        let reqs = parse_requires("(deploy a, b), lint");
        assert_eq!(req_names(&reqs), ["deploy", "lint"]);
        assert_eq!(reqs[0].args, ["a", "b"]);
    }

    #[test]
    fn a_quoted_argument_may_contain_a_space() {
        let reqs = parse_requires(r#"(deploy "the droplet, west" now)"#);
        assert_eq!(reqs[0].args, ["the droplet, west", "now"]);
    }

    /// An unterminated placeholder or quote is text, not a parse failure: this
    /// runs over hand-written markdown, and refusing to plan is worse than
    /// passing through what was actually typed.
    #[test]
    fn an_unterminated_placeholder_or_quote_is_taken_literally() {
        assert_eq!(parse_requires("(dist {{ module)")[0].args, ["{{ module"]);
        assert_eq!(parse_requires(r#"(dist "oops)"#)[0].args, ["oops"]);
    }

    #[test]
    fn crlf_scripts_are_normalized() {
        let tf = parse("## t\r\n\r\n```sh\r\necho foo\r\necho bar\r\n```\r\n");
        assert_eq!(tf.jobs[0].script, "echo foo\necho bar\n");
        assert!(!tf.jobs[0].script.contains('\r'));
    }

    #[test]
    fn an_unterminated_fence_warns_but_keeps_the_job() {
        let tf = parse("## a\n\n```sh\necho hi\n"); // no closing fence
        assert_eq!(tf.jobs.len(), 1);
        assert_eq!(tf.jobs[0].script.trim(), "echo hi");
        assert!(tf.warnings.iter().any(|w| w.contains("unterminated")));
    }

    #[test]
    fn a_stray_fence_open_does_not_close_an_unterminated_block() {
        // ```sh has an info string, so it opens rather than closes; only a bare
        // ``` closes. (The trailing block here is what closes it.)
        let tf = parse("## a\n\n```sh\none\n```sh\ntwo\n```\n");
        assert!(tf.jobs[0].script.contains("one"));
        assert!(tf.jobs[0].script.contains("```sh\ntwo"));
    }

    /// The one that matters. A sentence can be wrapped so that a line inside it
    /// reads as a metadata key, which let a paragraph opt its own task in to
    /// agent execution while reading as ordinary prose to every human reviewing
    /// the file. Metadata has to begin a block.
    #[test]
    fn metadata_inside_a_paragraph_does_not_configure_the_task() {
        let tf = parse(
            "## t\n\nThe reviewer decides whether to set\nAgent: allow\non a task.\n\n```sh\ntrue\n```\n",
        );
        assert!(!tf.jobs[0].agent_allow, "prose must not open the gate");
        assert!(
            tf.jobs[0].description.contains("Agent: allow"),
            "it is description, and stays visible as such"
        );
    }

    /// Silently ignoring it would be its own trap: an author who did mean it
    /// needs to hear that it did nothing.
    #[test]
    fn metadata_inside_a_paragraph_warns() {
        let tf = parse("## t\n\nRun this after\nRequires: build\n\n```sh\ntrue\n```\n");
        assert!(tf.jobs[0].requires.is_empty());
        assert!(
            tf.warnings.iter().any(|w| w.contains("inside a paragraph")),
            "warnings: {:?}",
            tf.warnings
        );
    }

    /// Metadata after prose is how nearly every real task file is written: a
    /// paragraph of description, a blank line, then `Args:`. The rule is about
    /// paragraph *interiors*, and must not break that.
    #[test]
    fn metadata_after_a_blank_line_still_works() {
        let tf = parse(
            "## t\n\nSet a module's version.\n\nArgs: module version\nRequires: lint\nAgent: allow\n\n```sh\ntrue\n```\n",
        );
        let j = &tf.jobs[0];
        assert_eq!(j.args.len(), 2, "after a blank line");
        assert_eq!(req_names(&j.requires), ["lint"], "and a run stays together");
        assert!(j.agent_allow);
        assert!(tf.warnings.is_empty(), "warnings: {:?}", tf.warnings);
    }

    #[test]
    fn metadata_directly_under_the_heading_still_works() {
        let tf = parse("## t\nArgs: one\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.jobs[0].args.len(), 1);
    }

    #[test]
    fn opens_list_item_recognizes_the_usual_markers() {
        for good in ["- a", "* a", "+ a", "1. a", "12) a", "  - indented"] {
            assert!(opens_list_item(good), "{good:?}");
        }
        for bad in ["-not a bullet", "a - b", "1.5 is a number", "", "text"] {
            assert!(!opens_list_item(bad), "{bad:?}");
        }
    }

    /// A description keeps its paragraph breaks, so a consumer can tell where
    /// the opening thought ends. Dropping blanks left one undifferentiated run
    /// of lines, and the only "summary" available was a hard-wrap fragment.
    #[test]
    fn a_description_keeps_its_paragraph_breaks() {
        let tf = parse("## t\n\nFirst thought,\nwrapped.\n\nSecond thought.\n\n```sh\ntrue\n```\n");
        let d = &tf.jobs[0].description;
        let paras: Vec<&str> = d.split("\n\n").filter(|p| !p.trim().is_empty()).collect();
        assert_eq!(paras.len(), 2, "description was {d:?}");
        assert_eq!(paras[0].trim(), "First thought,\nwrapped.");
    }

    #[test]
    fn indented_metadata_is_recognized() {
        let tf = parse("## a\n\n- steps:\n  Env: KEY=val\n\n```sh\ntrue\n```\n");
        assert_eq!(tf.jobs[0].env, vec![("KEY".into(), "val".into())]);
    }

    /// An unrecognized language is still a task, run as `sh`, and warned about.
    /// Forgiving is deliberate: `shell-session` and `bash5` should work.
    /// Repeated metadata accumulates. Assigning meant the second line silently
    /// erased the first, so a declared dependency never ran and the task still
    /// exited 0.
    #[test]
    fn repeated_metadata_lines_accumulate() {
        let tf = parse("## t\n\nRequires: alpha\nRequires: beta\n\n```sh\ntrue\n```\n");
        assert_eq!(req_names(&tf.jobs[0].requires), ["alpha", "beta"]);

        let tf = parse("## t\n\nArgs: a\nArgs: b\n\n```sh\ntrue\n```\n");
        let names: Vec<_> = tf.jobs[0].args.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    /// A near-miss of a real key is a typo, and used to vanish into the
    /// description with no warning at all.
    #[test]
    fn a_misspelled_metadata_key_warns() {
        for (typo, meant) in [("Require", "requires"), ("Arg", "args"), ("Opt", "opts")] {
            let src = format!("## t\n\n{typo}: x\n\n```sh\ntrue\n```\n");
            let tf = parse(&src);
            assert!(
                tf.warnings.iter().any(|w| w.contains(meant)),
                "{typo}: should suggest {meant}, warnings were {:?}",
                tf.warnings
            );
        }
    }

    /// ...but ordinary prose that happens to start with a word and a colon must
    /// stay prose, or the format cannot live in the documentation it claims to.
    #[test]
    fn ordinary_prose_is_not_mistaken_for_metadata() {
        let tf = parse(
            "## t\n\nNote: this is a sentence.\nWarning: so is this.\nSee: the docs.\n\n```sh\ntrue\n```\n",
        );
        assert!(
            tf.warnings.is_empty(),
            "prose should not warn, got {:?}",
            tf.warnings
        );
        assert!(tf.jobs[0].description.contains("Note: this is a sentence."));
    }

    #[test]
    fn an_unknown_language_is_still_a_task_and_warns() {
        let tf = parse("## a\n\n```shell-session\ntrue\n```\n");
        assert_eq!(tf.jobs.len(), 1);
        assert!(tf.warnings.iter().any(|w| w.contains("shell-session")));
        assert!(
            tf.warnings
                .iter()
                .any(|w| w.contains("running as a strict sh"))
        );
    }

    #[test]
    fn duplicate_names_warn_and_the_first_wins() {
        let tf = parse("## a\n\n```sh\necho one\n```\n\n## a\n\n```sh\necho two\n```\n");
        assert_eq!(tf.jobs.len(), 2);
        assert!(tf.jobs[0].script.contains("one"));
        assert!(tf.warnings.iter().any(|w| w.contains("duplicate")));
    }

    #[test]
    fn a_task_option_at_file_level_says_where_it_belongs() {
        let tf = parse("Opts: inherit-cwd\n\n## t\n\n```sh\ntrue\n```\n");
        assert!(!tf.includes_parent());
        assert!(
            tf.warnings
                .iter()
                .any(|w| w.contains("task option belongs")),
            "warnings: {:?}",
            tf.warnings
        );
    }

    #[test]
    fn a_file_option_under_a_task_says_where_it_belongs() {
        let tf = parse("## t\n\nOpts: include-parent\n\n```sh\ntrue\n```\n");
        assert!(
            tf.warnings.iter().any(|w| w.contains("file-level")),
            "warnings: {:?}",
            tf.warnings
        );
    }

    #[test]
    fn no_strict_is_a_known_opt_and_warns_no_one() {
        let tf = parse("## t\n\nOpts: no-strict\n\n```sh\ntrue\n```\n");
        assert!(
            tf.warnings().is_empty(),
            "no-strict must be recognized: {:?}",
            tf.warnings()
        );
    }
}
