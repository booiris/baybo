use brush_parser::ParserOptions;
use brush_parser::ast;
use brush_parser::word::{self, WordPiece, WordPieceWithSource};

/// argv0 of the command string — the first whitespace-separated token,
/// path-stripped (`/usr/bin/ls` → `ls`). Does NOT skip wrappers (`xargs`,
/// `timeout`, `nohup`, …) and does NOT skip `KEY=VAL` env-var prefixes,
/// because callers (delete detection, FileToolRedirect classification,
/// exit-code interpretation) only care about the literal leading token.
pub(super) fn first_token(command: &str) -> Option<&str> {
    let raw = command.split_whitespace().next()?;
    let bare = raw.trim_start_matches('\\');
    if bare.contains('=') && !bare.starts_with('=') {
        return None;
    }
    Some(strip_path(bare))
}

fn strip_path(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// argv0s that delete files. Matched against the resolved command-position
/// argv0 of every command in the parsed AST (after wrapper/env unwrapping)
/// and against a `find` `-exec` target. `mv` is intentionally absent — it
/// relocates rather than destroys; `dd`, `truncate`, and `>` redirection
/// can also wipe data but are too noisy to gate on. Path-prefixed forms
/// (`/usr/bin/rm`) are normalized through `strip_path` before matching.
/// `-delete` is the `find` primary, not an argv0, so it lives outside this
/// list ([`find_args_are_destructive`]).
const STANDALONE_DELETE_TOKENS: &[&str] = &["rm", "rmdir", "unlink", "shred", "srm", "wipe"];

/// Wrappers whose own argv0 is benign but which exec another command taken
/// from their arguments. [`resolve_wrapped`] skips each wrapper's options
/// (and the values of the ones that take one) to find the single real
/// command position, so a delete token that is merely an argument to the
/// wrapper (`xargs grep rm`, `sudo pacman -S wipe`, `command -v rm`) is not
/// mistaken for the wrapped command.
const WRAPPER_COMMANDS: &[&str] = &[
    "xargs", "nohup", "nice", "ionice", "timeout", "sudo", "doas", "env", "command", "exec",
];

/// True when ANY command in `command` performs a destructive operation:
/// a standalone delete argv0 (`rm`/`rmdir`/…), `find -delete`/`-exec rm`,
/// or a known destructive `git` invocation (see
/// [`git_args_are_destructive`]). Used by `accessed_resources` to decide
/// whether the pre-execution approval gate should fire.
///
/// The command is parsed into a real bash AST ([`brush_parser`]) and every
/// simple command — including ones nested in pipelines, `&&`/`||` lists,
/// subshells, `if`/`for`/`while`/`case` bodies, functions, and `$(…)`/
/// backtick command substitutions — is matched at its resolved *command
/// position*. Checking only the argv0 (after wrapper/env unwrapping) is
/// what keeps a delete token that appears as a grep pattern, a filename
/// operand, a heredoc body, or a bare argument from tripping the gate.
pub(super) fn contains_delete_command(command: &str) -> bool {
    match parse_program(command) {
        Some(program) => program_is_destructive(&program, MAX_PARSE_DEPTH),
        // A genuine syntax error (unbalanced quote, or a construct brush
        // can't parse). Fail closed — but only if the raw text could even
        // be a delete. Every real destructive command contains one of
        // these substrings, so a benign command that merely failed to
        // parse and mentions no delete/git keyword is not forced to prompt.
        None => raw_mentions_delete_keyword(command),
    }
}

/// Recursion budget shared by AST descent and command-substitution
/// re-parsing. Deep enough for any real command; a pathologically nested
/// input that exhausts it fails closed to a prompt rather than looping.
const MAX_PARSE_DEPTH: usize = 32;

/// Trace `action` label for destructive-scan events (currently the
/// `ToolEventPayload::ParseFailure` marker emitted from `execute`).
pub(super) const DELETE_SCAN_EVENT_ACTION: &str = "delete_scan";

/// Cap a command string recorded in a trace event so a large heredoc body
/// doesn't bloat the span; the head (which carries the argv0) is kept.
const MAX_EVENT_COMMAND_CHARS: usize = 2_000;

pub(super) fn truncate_for_event(command: &str) -> String {
    if command.chars().count() <= MAX_EVENT_COMMAND_CHARS {
        return command.to_string();
    }
    let head: String = command.chars().take(MAX_EVENT_COMMAND_CHARS).collect();
    format!("{head}…(truncated)…")
}

pub(super) fn parse_program(command: &str) -> Option<ast::Program> {
    let reader = std::io::BufReader::new(command.as_bytes());
    let mut parser = brush_parser::Parser::new(reader, &ParserOptions::default());
    parser.parse_program().ok()
}

/// Cheap substring pre-filter for the parse-error fail-closed path. Any
/// argv0 we treat as destructive (or its `git`/`find -delete` forms)
/// necessarily leaves one of these substrings in the raw command, so a
/// miss here can only be a command that could not have been a delete.
fn raw_mentions_delete_keyword(command: &str) -> bool {
    STANDALONE_DELETE_TOKENS.iter().any(|t| command.contains(t))
        || command.contains("-delete")
        || command.contains("git")
}

fn program_is_destructive(program: &ast::Program, depth: usize) -> bool {
    program
        .complete_commands
        .iter()
        .any(|list| list_is_destructive(list, depth))
}

fn list_is_destructive(list: &ast::CompoundList, depth: usize) -> bool {
    list.0
        .iter()
        .any(|ast::CompoundListItem(and_or, _)| and_or_is_destructive(and_or, depth))
}

fn and_or_is_destructive(and_or: &ast::AndOrList, depth: usize) -> bool {
    pipeline_is_destructive(&and_or.first, depth)
        || and_or.additional.iter().any(|ao| match ao {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => pipeline_is_destructive(p, depth),
        })
}

fn pipeline_is_destructive(pipeline: &ast::Pipeline, depth: usize) -> bool {
    pipeline
        .seq
        .iter()
        .any(|command| command_is_destructive(command, depth))
}

fn command_is_destructive(command: &ast::Command, depth: usize) -> bool {
    match command {
        ast::Command::Simple(simple) => simple_is_destructive(simple, depth),
        ast::Command::Compound(compound, redirs) => {
            compound_is_destructive(compound, depth) || redirect_list_is_destructive(redirs, depth)
        }
        ast::Command::Function(f) => {
            compound_is_destructive(&f.body.0, depth)
                || redirect_list_is_destructive(&f.body.1, depth)
        }
        // `[[ … ]]` never deletes on its own, but Bash expands command
        // substitutions in its operands before evaluating, so scan them.
        ast::Command::ExtendedTest(test, redirs) => {
            extended_test_is_destructive(&test.expr, depth)
                || redirect_list_is_destructive(redirs, depth)
        }
    }
}

/// Redirections attached to a compound command, function, or extended test
/// (e.g. `while …; done < <(rm …)`) carry the same executable side effects
/// as a simple command's redirections.
fn redirect_list_is_destructive(redirs: &Option<ast::RedirectList>, depth: usize) -> bool {
    redirs
        .iter()
        .flat_map(|r| &r.0)
        .any(|redir| io_redirect_is_destructive(redir, depth))
}

/// `[[ -n $(rm …) ]]` / `[[ $(rm …) == x ]]`: the test itself is harmless,
/// but every operand word is expanded first, so any command substitution
/// inside it runs.
fn extended_test_is_destructive(expr: &ast::ExtendedTestExpr, depth: usize) -> bool {
    use ast::ExtendedTestExpr as E;
    match expr {
        E::And(a, b) | E::Or(a, b) => {
            extended_test_is_destructive(a, depth) || extended_test_is_destructive(b, depth)
        }
        E::Not(e) | E::Parenthesized(e) => extended_test_is_destructive(e, depth),
        E::UnaryTest(_, w) => word_has_destructive_subst(&w.value, depth),
        E::BinaryTest(_, a, b) => {
            word_has_destructive_subst(&a.value, depth)
                || word_has_destructive_subst(&b.value, depth)
        }
    }
}

fn compound_is_destructive(compound: &ast::CompoundCommand, depth: usize) -> bool {
    let Some(depth) = depth.checked_sub(1) else {
        return true; // nesting past the budget: fail closed
    };
    use ast::CompoundCommand as C;
    match compound {
        C::BraceGroup(b) => list_is_destructive(&b.list, depth),
        C::Subshell(s) => list_is_destructive(&s.list, depth),
        // The `in` words are expanded before the loop body runs, so a
        // `for f in $(rm …); do …` deletes even with a harmless body.
        C::ForClause(f) => {
            f.values
                .iter()
                .flatten()
                .any(|w| word_has_destructive_subst(&w.value, depth))
                || list_is_destructive(&f.body.list, depth)
        }
        C::ArithmeticForClause(f) => {
            [&f.initializer, &f.condition, &f.updater]
                .into_iter()
                .flatten()
                .any(|e| word_has_destructive_subst(&e.value, depth))
                || list_is_destructive(&f.body.list, depth)
        }
        C::WhileClause(w) | C::UntilClause(w) => {
            list_is_destructive(&w.0, depth) || list_is_destructive(&w.1.list, depth)
        }
        C::IfClause(i) => {
            list_is_destructive(&i.condition, depth)
                || list_is_destructive(&i.then, depth)
                || i.elses.iter().flatten().any(|e| {
                    e.condition
                        .as_ref()
                        .is_some_and(|c| list_is_destructive(c, depth))
                        || list_is_destructive(&e.body, depth)
                })
        }
        // The case subject word and each branch's patterns are expanded
        // before matching (`case $(rm …) in`, `case x in $(rm …))`).
        C::CaseClause(c) => {
            word_has_destructive_subst(&c.value.value, depth)
                || c.cases.iter().any(|item| {
                    item.patterns
                        .iter()
                        .any(|p| word_has_destructive_subst(&p.value, depth))
                        || item
                            .cmd
                            .as_ref()
                            .is_some_and(|l| list_is_destructive(l, depth))
                })
        }
        // A coprocess runs its body command (asynchronously).
        C::Coprocess(c) => command_is_destructive(&c.body, depth),
        // `(( … ))` evaluates numbers, but the expression is expanded first,
        // so a command substitution in it (`(( $(rm …) ))`) still runs.
        C::Arithmetic(a) => word_has_destructive_subst(&a.expr.value, depth),
    }
}

fn simple_is_destructive(simple: &ast::SimpleCommand, depth: usize) -> bool {
    // Any word (command name, arguments, or assignment value) may embed a
    // `$(…)` / backtick command substitution whose body runs a delete.
    if simple_command_words(simple)
        .iter()
        .any(|w| word_has_destructive_subst(&w.value, depth))
    {
        return true;
    }

    // Process substitutions and expandable redirections run their own
    // commands that never appear in the argv: `cat <(rm …)`, `cmd > >(rm …)`,
    // `<<EOF … $(rm …) … EOF`, `<<< $(rm …)`.
    if simple
        .prefix
        .iter()
        .flat_map(|p| &p.0)
        .chain(simple.suffix.iter().flat_map(|s| &s.0))
        .any(|item| item_side_effect_is_destructive(item, depth))
    {
        return true;
    }

    // Command-position check: build the dequoted argv (name + argument
    // words only) and match the resolved argv0.
    let mut argv: Vec<String> = Vec::new();
    if let Some(name) = &simple.word_or_name {
        argv.push(dequote(&name.value));
    }
    if let Some(ast::CommandSuffix(items)) = &simple.suffix {
        for item in items {
            if let ast::CommandPrefixOrSuffixItem::Word(w) = item {
                argv.push(dequote(&w.value));
            }
        }
    }
    argv_is_destructive(&argv, depth)
}

/// A prefix/suffix element that executes a nested command as a side effect
/// (process substitution or an expandable redirection target), so its
/// contents are checked even though it is not part of the argv.
fn item_side_effect_is_destructive(item: &ast::CommandPrefixOrSuffixItem, depth: usize) -> bool {
    use ast::CommandPrefixOrSuffixItem as Item;
    match item {
        Item::ProcessSubstitution(_, subshell) => list_is_destructive(&subshell.list, depth),
        Item::IoRedirect(redir) => io_redirect_is_destructive(redir, depth),
        // Word / AssignmentWord substitutions are covered by the
        // `simple_command_words` scan in `simple_is_destructive`.
        Item::Word(_) | Item::AssignmentWord(..) => false,
    }
}

fn io_redirect_is_destructive(redir: &ast::IoRedirect, depth: usize) -> bool {
    use ast::IoRedirect as R;
    match redir {
        R::File(_, _, target) => match target {
            ast::IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                list_is_destructive(&subshell.list, depth)
            }
            ast::IoFileRedirectTarget::Filename(w) | ast::IoFileRedirectTarget::Duplicate(w) => {
                word_has_destructive_subst(&w.value, depth)
            }
            ast::IoFileRedirectTarget::Fd(_) => false,
        },
        // Only an *unquoted* heredoc delimiter (`<<EOF`) expands the body; a
        // quoted one (`<<'EOF'`) is literal text and never executed.
        R::HereDocument(_, hd) => {
            hd.requires_expansion && word_has_destructive_subst(&hd.doc.value, depth)
        }
        R::HereString(_, w) => word_has_destructive_subst(&w.value, depth),
        R::OutputAndError(w, _) => word_has_destructive_subst(&w.value, depth),
    }
}

/// Every word attached to a simple command that may carry a command
/// substitution: the name, argument words, and the values of `KEY=val`
/// assignment prefixes.
fn simple_command_words(simple: &ast::SimpleCommand) -> Vec<&ast::Word> {
    let mut words: Vec<&ast::Word> = Vec::new();
    if let Some(name) = &simple.word_or_name {
        words.push(name);
    }
    let sections = [
        simple.prefix.as_ref().map(|p| &p.0),
        simple.suffix.as_ref().map(|s| &s.0),
    ];
    for items in sections.into_iter().flatten() {
        for item in items {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(w)
                | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => words.push(w),
                _ => {}
            }
        }
    }
    words
}

fn word_has_destructive_subst(value: &str, depth: usize) -> bool {
    let Some(depth) = depth.checked_sub(1) else {
        return true; // nesting past the budget: fail closed like the AST path
    };
    match word::parse(value, &ParserOptions::default()) {
        Ok(pieces) => pieces_have_destructive_subst(value, &pieces, depth),
        Err(_) => false,
    }
}

fn pieces_have_destructive_subst(src: &str, pieces: &[WordPieceWithSource], depth: usize) -> bool {
    pieces.iter().any(|p| match &p.piece {
        WordPiece::CommandSubstitution(inner) | WordPiece::BackquotedCommandSubstitution(inner) => {
            match parse_program(inner) {
                Some(program) => program_is_destructive(&program, depth),
                None => raw_mentions_delete_keyword(inner),
            }
        }
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            pieces_have_destructive_subst(src, inner, depth)
        }
        // A `${…}` parameter expansion or `$(( … ))` arithmetic expansion
        // stores its body as raw text, so a command substitution nested in
        // a default value / pattern / operand (`${x:-$(rm …)}`,
        // `$(( $(rm …) ))`) is not a top-level piece. Re-parse the inner
        // text to reach it.
        WordPiece::ParameterExpansion(_) | WordPiece::ArithmeticExpression(_) => src
            .get(p.start_index..p.end_index)
            .and_then(expansion_body)
            .is_some_and(|inner| word_has_destructive_subst(inner, depth)),
        _ => false,
    })
}

/// Strip the outer `${…}` / `$((…))` / `$[…]` syntax off an expansion's
/// source text, returning the inner body to re-scan for command
/// substitutions. A brace-less `$VAR` (no wrapper to strip) yields `None`.
fn expansion_body(src: &str) -> Option<&str> {
    if let Some(inner) = src.strip_prefix("$((").and_then(|s| s.strip_suffix("))")) {
        return Some(inner);
    }
    if let Some(inner) = src.strip_prefix("$[").and_then(|s| s.strip_suffix(']')) {
        return Some(inner);
    }
    src.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
}

/// Reproduce what `sh -c` would exec for a single word: strip surrounding
/// quotes / backslash escapes (`'rm'`, `"rm"`, `/bin/'rm'`, `\rm`,
/// `"r""m"` → `rm`, `/bin/rm`). Words that are not a single clean literal
/// (a `$(…)` expansion, an empty split) are returned unchanged — they
/// cannot equal a delete token, and any substitution they carry is caught
/// separately by [`word_has_destructive_subst`].
fn dequote(value: &str) -> String {
    match shell_words::split(value) {
        Ok(mut words) if words.len() == 1 => words.pop().unwrap_or_default(),
        _ => value.to_string(),
    }
}

/// True when the resolved command-position argv is a delete: a standalone
/// delete argv0, a destructive `git`/`find` invocation, an interpreter that
/// runs a delete given as a command string ([`command_string_is_destructive`]),
/// or a wrapper ([`resolve_wrapped`]) around one of those.
fn argv_is_destructive(argv: &[String], depth: usize) -> bool {
    let Some((argv0, rest)) = argv.split_first() else {
        return false;
    };
    let bare = strip_path(argv0);
    if STANDALONE_DELETE_TOKENS.contains(&bare) {
        return true;
    }
    if command_string_is_destructive(bare, rest, depth) {
        return true;
    }
    match bare {
        "git" => {
            let args: Vec<&str> = rest.iter().map(String::as_str).collect();
            git_args_are_destructive(&args)
        }
        "find" => find_args_are_destructive(rest, depth),
        _ if WRAPPER_COMMANDS.contains(&bare) => {
            resolve_wrapped(bare, rest).is_some_and(|inner| argv_is_destructive(inner, depth))
        }
        _ => false,
    }
}

/// Interpreters that take the command to run as a shell STRING argument
/// rather than as separate argv tokens: `sh`/`bash`/… `-c "…"`, `env -S
/// "…"`, and `eval …`. The string is parsed and checked recursively so a
/// delete inside it is not skipped as an opaque option value.
fn command_string_is_destructive(argv0: &str, rest: &[String], depth: usize) -> bool {
    let deq: Vec<String> = rest.iter().map(|s| dequote(s)).collect();
    match argv0 {
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" | "ash" => {
            command_string_after_c_flag(&deq).is_some_and(|s| string_is_destructive(s, depth))
        }
        "env" => split_string_value(&deq).is_some_and(|s| string_is_destructive(s, depth)),
        // `eval a b c` runs the space-joined concatenation of its arguments.
        "eval" => string_is_destructive(&deq.join(" "), depth),
        _ => false,
    }
}

/// Parse a shell command string (a `-c` / `-S` / `eval` operand) and test
/// it with the full detector. A parse failure falls back to the keyword
/// pre-filter; budget exhaustion fails closed, matching the AST path.
fn string_is_destructive(s: &str, depth: usize) -> bool {
    let Some(depth) = depth.checked_sub(1) else {
        return true;
    };
    match parse_program(s) {
        Some(program) => program_is_destructive(&program, depth),
        None => raw_mentions_delete_keyword(s),
    }
}

/// The command-string operand following a `-c` flag (bare `-c` or a combined
/// short group containing `c`, e.g. `-lc`).
fn command_string_after_c_flag(args: &[String]) -> Option<&str> {
    let is_c_flag = |a: &str| {
        a == "-c"
            || (a.starts_with('-')
                && !a.starts_with("--")
                && a.len() > 1
                && a[1..].chars().all(|c| c.is_ascii_alphabetic())
                && a[1..].contains('c'))
    };
    let idx = args.iter().position(|a| is_c_flag(a))?;
    args.get(idx + 1).map(String::as_str)
}

/// The command-string operand of `env`'s `-S` / `--split-string` option, in
/// separate (`-S str`), attached (`-Sstr`), or `=` (`--split-string=str`)
/// form.
fn split_string_value(args: &[String]) -> Option<&str> {
    for (i, a) in args.iter().enumerate() {
        if a == "-S" || a == "--split-string" {
            return args.get(i + 1).map(String::as_str);
        }
        if let Some(v) = a.strip_prefix("--split-string=") {
            return Some(v);
        }
        if let Some(v) = a.strip_prefix("-S").filter(|v| !v.is_empty()) {
            return Some(v);
        }
    }
    None
}

/// `find … -delete` and `find … -exec <destructive> …` delete files.
/// `-exec`'s operand is itself a full argv (a wrapper or `rm` directly),
/// so it recurses through [`argv_is_destructive`].
fn find_args_are_destructive(args: &[String], depth: usize) -> bool {
    if args.iter().any(|a| a == "-delete") {
        return true;
    }
    args.iter().enumerate().any(|(i, a)| {
        matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")
            && argv_is_destructive(&args[i + 1..], depth)
    })
}

/// Resolve the command a wrapper actually executes, returning the argv
/// slice that begins at the wrapped command's own argv0. Each wrapper's
/// own options (and the values of the ones that take one) are skipped so a
/// delete token that is merely an *argument* to the wrapper — `command -v
/// rm`, `sudo pacman -S wipe`, `xargs grep rm` — is not mistaken for the
/// wrapped command. Returns `None` for a bare wrapper or a lookup-only
/// `command -v`.
fn resolve_wrapped<'a>(wrapper: &str, rest: &'a [String]) -> Option<&'a [String]> {
    let is_flag = |a: &str| a.starts_with('-') && a.len() > 1;
    let idx = match wrapper {
        "command" => {
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    // `-v`/`-V` make this a lookup query, not an execution.
                    "-v" | "-V" => return None,
                    a if is_flag(a) => i += 1,
                    _ => break,
                }
            }
            i
        }
        "env" => {
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    // `-a NAME` overrides argv0, `-u`/`-C` take a value — the
                    // wrapped command follows *after* that value. (`-S` is a
                    // command string, handled by `command_string_is_destructive`.)
                    "-u" | "--unset" | "-C" | "--chdir" | "-a" | "--argv0" => i += 2,
                    a if is_flag(a) => i += 1,
                    a if is_env_assignment(a) => i += 1,
                    _ => break,
                }
            }
            i
        }
        "xargs" => {
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    // Only the required-argument forms consume the next
                    // token. The deprecated `-i`/`-l`/`-e` (and their long
                    // synonyms) take an OPTIONAL operand that must be
                    // attached (`-i{}`), so a separate token after them is
                    // the command to run (`xargs -i rm`) — those fall to
                    // the generic single-token arm.
                    "-I" | "-n" | "-P" | "-L" | "-d" | "-E" | "-s" | "-a" | "--max-args"
                    | "--max-procs" | "--delimiter" | "--max-chars" | "--arg-file" => i += 2,
                    a if is_flag(a) => i += 1,
                    _ => break,
                }
            }
            i
        }
        "timeout" => {
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    "-s" | "--signal" | "-k" | "--kill-after" => i += 2,
                    a if is_flag(a) => i += 1,
                    _ => break,
                }
            }
            // Skip the DURATION operand that precedes the command.
            i + 1
        }
        "nice" | "ionice" => {
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    "-n" | "--adjustment" | "-c" | "-p" | "-t" => i += 2,
                    a if is_flag(a) => i += 1,
                    _ => break,
                }
            }
            i
        }
        "sudo" | "doas" => {
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    // Options that take a SEPARATE value (`--user root`);
                    // the `--user=root` form is self-contained and skipped
                    // by the generic single-token arm below.
                    "-u" | "--user" | "-g" | "--group" | "-U" | "--other-user" | "-C"
                    | "--close-from" | "-D" | "--chdir" | "-h" | "--host" | "-p" | "--prompt"
                    | "-r" | "--role" | "-t" | "--type" | "-R" | "--chroot" | "-T"
                    | "--command-timeout" => i += 2,
                    a if is_flag(a) => i += 1,
                    _ => break,
                }
            }
            i
        }
        "exec" => {
            // `exec [-cl] [-a name] command`: `-a` sets a custom argv0 and
            // takes a separate value, so `exec -a custom rm …` still runs rm.
            let mut i = 0;
            while let Some(a) = rest.get(i).map(String::as_str) {
                match a {
                    "-a" => i += 2,
                    a if is_flag(a) => i += 1,
                    _ => break,
                }
            }
            i
        }
        // nohup / any other wrapper: skip only leading option flags.
        _ => {
            let mut i = 0;
            while rest.get(i).is_some_and(|a| is_flag(a)) {
                i += 1;
            }
            i
        }
    };
    rest.get(idx..).filter(|s| !s.is_empty())
}

/// True if `tok` looks like a leading `KEY=value` env assignment.
/// Bash treats one or more such tokens at the start of a sub-command as
/// per-invocation env overrides, with the actual argv0 starting after
/// them. We use the conservative shape `[A-Za-z_][A-Za-z0-9_]*=…`.
pub(super) fn is_env_assignment(tok: &str) -> bool {
    let Some(eq) = tok.find('=') else {
        return false;
    };
    let key = &tok[..eq];
    if key.is_empty() {
        return false;
    }
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `args` is the slice after the literal `git` token. Skip leading
/// global options (which can take a separate value or embed one with
/// `=`), then match known destructive subcommand+flag combinations.
/// The match arms intentionally stay narrow — `git checkout .`, `git
/// restore`, `git stash pop`, and `git reset --soft` rewrite working
/// state but don't strictly delete, and gating them under approval is
/// noisy enough to outweigh the safety win.
fn git_args_are_destructive(args: &[&str]) -> bool {
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        if matches!(
            a,
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
        ) {
            i += 2;
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        if matches!(a, "-p" | "-P") {
            i += 1;
            continue;
        }
        break;
    }
    let Some(&sub) = args.get(i) else {
        return false;
    };
    let rest = &args[i + 1..];
    match sub {
        "clean" => rest.iter().any(|a| is_git_clean_destructive_flag(a)),
        "reset" => rest.contains(&"--hard"),
        "branch" => rest
            .iter()
            .any(|a| matches!(*a, "-d" | "-D" | "--delete" | "--delete-merged")),
        "tag" => rest.iter().any(|a| matches!(*a, "-d" | "--delete")),
        "push" => rest.iter().any(|a| {
            matches!(
                *a,
                "-f" | "-d" | "--force" | "--force-with-lease" | "--delete"
            ) || a.starts_with("--force-with-lease=")
                || a.starts_with("--force-if-includes")
        }),
        "stash" => matches!(rest.first().copied(), Some("drop") | Some("clear")),
        "worktree" => matches!(rest.first().copied(), Some("remove")),
        "update-ref" => rest.iter().any(|a| matches!(*a, "-d" | "--delete")),
        "filter-branch" | "filter-repo" => true,
        // `git rm <pathspec>` deletes from the working tree. `--cached`
        // only unstages, and `-n`/`--dry-run` change nothing on disk.
        "rm" => !rest
            .iter()
            .any(|a| matches!(*a, "--cached" | "-n" | "--dry-run")),
        _ => false,
    }
}

/// `git clean` only deletes when invoked with `-f`/`--force` (or a
/// combined short flag containing `f`, e.g. `-fd`, `-fdx`). `-n` /
/// `--dry-run` and `-i` / `--interactive` alone don't delete; we don't
/// model `-n` cancelling `-f` — a false-positive prompt for a dry-run
/// that also passes `-f` is acceptable.
fn is_git_clean_destructive_flag(arg: &str) -> bool {
    if arg == "-f" || arg == "--force" {
        return true;
    }
    if arg.starts_with("--") {
        return false;
    }
    if let Some(rest) = arg.strip_prefix('-') {
        return !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_alphabetic())
            && rest.contains('f');
    }
    false
}

/// Lightweight tokenizer that respects single- and double-quoted regions
/// and splits the command into one token-list per sub-command. A
/// sub-command boundary is `;`, `|`, `&`, `(`, `)`, or backtick (the
/// last splits even inside double quotes, since command substitution
/// fires there). Whitespace is a token boundary that stays inside the
/// current sub-command. Not a full shell parser — variable expansion,
/// `$(…)` content, and escapes inside single quotes are not
/// interpreted; the goal is "find argv0s per sub-command, well enough
/// to drive a security heuristic". The `$` of `$(…)` is left as a
/// stray token in the outer sub-command and the inner contents form
/// their own sub-command, which is enough for delete-detection.
pub(super) fn split_into_subcommands(command: &str) -> Vec<Vec<&str>> {
    let bytes = command.as_bytes();
    let mut subs: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_single && b == b'\\' && i + 1 < bytes.len() {
            if start.is_none() {
                start = Some(i);
            }
            i += 2;
            continue;
        }
        if !in_double && b == b'\'' {
            if start.is_none() {
                start = Some(i);
            }
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            if start.is_none() {
                start = Some(i);
            }
            in_double = !in_double;
            i += 1;
            continue;
        }
        let outside_quotes = !in_single && !in_double;
        let is_subcmd_separator = (outside_quotes && matches!(b, b';' | b'|' | b'&' | b'(' | b')'))
            || (!in_single && b == b'`');
        let is_token_separator = outside_quotes && b.is_ascii_whitespace();
        if is_subcmd_separator {
            if let Some(s) = start.take() {
                current.push(&command[s..i]);
            }
            if !current.is_empty() {
                subs.push(std::mem::take(&mut current));
            }
        } else if is_token_separator {
            if let Some(s) = start.take() {
                current.push(&command[s..i]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
        i += 1;
    }
    if let Some(s) = start {
        current.push(&command[s..]);
    }
    if !current.is_empty() {
        subs.push(current);
    }
    subs
}
