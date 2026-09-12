/// A config with one module, so a button test says only what it is about.
fn one_module(body: &str) -> String {
    format!(
        r#"
[bar]
height = 30

[right]
groups = ["g"]

[group.g]
modules = ["m"]

[module.m]
{body}
"#
    )
}

/// How a command module's program is run, given what its `interval` says.
fn run_of(interval: &str) -> crate::collect::command::Run {
    let body = match interval.is_empty() {
        true => "source = \"command\"\ncommand = [\"true\"]".to_string(),
        false => {
            format!("source = \"command\"\ncommand = [\"true\"]\ninterval = \"{interval}\"")
        }
    };
    let cfg = Config::parse(&one_module(&body)).expect("a command module");
    match &cfg.modules().next().unwrap().source {
        Source::Native(Which::Command(spec)) => spec.run,
        other => panic!("source is {other:?}"),
    }
}

#[test]
fn a_command_with_no_interval_streams() {
    use crate::collect::command::Run;
    // The cheapest arrangement, and so the one you get by saying nothing: no process
    // is started to find out that nothing changed.
    assert_eq!(run_of(""), Run::Stream);
}

#[test]
fn an_interval_runs_the_command_again_that_often() {
    use crate::collect::command::Run;
    assert_eq!(run_of("30s"), Run::Every(Duration::from_secs(30)));
    assert_eq!(run_of("2m"), Run::Every(Duration::from_secs(120)));
}

#[test]
fn once_runs_it_at_startup_and_then_only_when_asked() {
    use crate::collect::command::Run;
    assert_eq!(run_of("once"), Run::Once);
    assert_eq!(run_of("ONCE"), Run::Once, "case is not the point");
}

#[test]
fn an_interval_that_would_never_stop_running_is_refused() {
    // A command run every no-time forks as fast as the machine can. Durations are
    // already required to be positive, which covers it.
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"true\"]\ninterval = \"0s\"",
    ))
    .expect_err("a zero interval never stops running");
    assert!(format!("{e:#}").contains("positive"), "{e:#}");
}

#[test]
fn an_interval_that_is_neither_a_period_nor_once_says_so() {
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"true\"]\ninterval = \"sometimes\"",
    ))
    .expect_err("that is not a schedule");
    let message = format!("{e:#}");
    assert!(message.contains("once"), "{message}");
}

#[test]
fn the_same_program_on_two_schedules_is_two_programs() {
    // Identity is the argv and how it is run: two modules that want the same script
    // at different rates want two of it, and sharing one would give one of them the
    // other's schedule.
    use crate::collect::command::Run;
    let a = crate::collect::CommandSpec {
        argv: vec!["s".to_string()],
        run: Run::Every(Duration::from_secs(1)),
        pages: false,
        fields: crate::collect::command::PLAIN,
        timeout: DEFAULT_COMMAND_TIMEOUT,
    };
    let b = crate::collect::CommandSpec {
        run: Run::Every(Duration::from_secs(60)),
        ..a.clone()
    };
    let same = a.clone();
    assert_ne!(a, b);
    assert_eq!(a, same);

    // And so is the deadline: one module willing to wait a minute for a script and
    // another that is not are asking for different things from the same program.
    let patient = crate::collect::CommandSpec {
        timeout: Duration::from_secs(60),
        ..a.clone()
    };
    assert_ne!(a, patient);
}

/// The list `--fields` answers from and the list a module may name have to be the
/// same list. They are written out separately - one is a table, the other a match -
/// so this is what stops them drifting apart, which would be help that lies.
#[test]
fn every_source_a_module_can_name_is_one_the_help_lists() {
    for (name, listed) in sources() {
        let body = match name {
            "command" => format!("source = \"{name}\"\ncommand = [\"true\"]"),
            _ => format!("source = \"{name}\""),
        };
        let cfg = Config::parse(&one_module(&body))
            .unwrap_or_else(|e| panic!("a module reading from {name:?}: {e:#}"));
        let resolved = &cfg.modules().next().expect("one module").source;
        let listed: Vec<&str> = listed.fields().iter().map(|f| f.name).collect();
        let real: Vec<&str> = resolved.fields().iter().map(|f| f.name).collect();
        assert_eq!(listed, real, "{name} publishes something else than it says");
    }
}

/// Two modules on one command share a process, so they have to share a schema too:
/// what only one of them declared would otherwise be parsed out of the output and
/// thrown away, leaving a module that validated and then drew nothing.
#[test]
fn modules_sharing_a_command_share_every_field_they_declared() {
    let text = r#"
[bar]
height = 30

[right]
groups = ["g"]

[group.g]
modules = ["temp", "wind"]

[module.temp]
source = "command"
command = ["weather"]
interval = "10m"
fields = { temp = "number" }
format = "$temp"

[module.wind]
source = "command"
command = ["weather"]
interval = "10m"
fields = { wind = "number" }
format = "$wind"
"#;
    let cfg = Config::parse(text).expect("two modules on one command");
    for module in cfg.modules() {
        let Source::Native(Which::Command(spec)) = &module.source else {
            panic!("{} is not a command module", module.name);
        };
        let names: Vec<&str> = spec.fields.iter().map(|f| f.name).collect();
        assert_eq!(names, ["temp", "wind"], "in module {:?}", module.name);
    }

    // And one command, not two: the schemas differing is not what tells two of them
    // apart.
    assert_eq!(cfg.collectors().len(), 1);
}

/// Sharing a reading means agreeing about what is in it. Two modules that do not are
/// told so by name, rather than one of them silently deciding.
#[test]
fn modules_sharing_a_command_may_not_disagree_about_a_field() {
    let text = r#"
[bar]
height = 30

[right]
groups = ["g"]

[group.g]
modules = ["a", "b"]

[module.a]
source = "command"
command = ["weather"]
interval = "10m"
fields = { load = "number" }
format = "$load"

[module.b]
source = "command"
command = ["weather"]
interval = "10m"
fields = { load = "text" }
format = "$load"
"#;
    let e = Config::parse(text).expect_err("two kinds for one field");
    let message = format!("{e:#}");
    assert!(
        message.contains("\"a\"") && message.contains("\"b\""),
        "{message}"
    );
    assert!(message.contains("load"), "{message}");
}

/// A bar that draws nothing the compositor knows must not connect to it: two sockets,
/// two threads and a tree read on every window title are the cost of asking.
#[test]
fn a_config_says_which_halves_of_the_compositor_it_needs() {
    let native = Config::parse(
        "[bar]\nheight = 30\n\n[right]\ngroups = [\"g\"]\n\n\
             [group.g]\nmodules = [\"cpu\"]\n\n[module.cpu]\nsource = \"cpu\"\n",
    )
    .expect("a native config");
    assert!(!native.needs_windows());
    assert!(!native.needs_workspaces());
    assert!(!native.needs_language());
    assert!(!native.needs_mode());

    let desktop = Config::parse(
        "[bar]\nheight = 30\n\n[right]\ngroups = [\"g\"]\n\n\
             [group.g]\nmodules = [\"ws\"]\n\n[module.ws]\nsource = \"sway:workspaces\"\n",
    )
    .expect("a workspace config");
    // A workspace list is not a window title: the tree is what costs, and nothing here
    // reads it.
    assert!(desktop.needs_workspaces());
    assert!(!desktop.needs_windows());
}

/// The argv a command module ends up with, given what it wrote.
fn argv_of(body: &str) -> Vec<String> {
    let cfg = Config::parse(&one_module(body)).expect("a command module");
    match &cfg.modules().next().unwrap().source {
        Source::Native(Which::Command(spec)) => spec.argv.clone(),
        other => panic!("source is {other:?}"),
    }
}

/// The knobs are arguments, in the order they were written: what they mean is the
/// script's business, which is what keeps one script good for two cities.
#[test]
fn params_are_handed_to_the_command_as_further_arguments() {
    let argv = argv_of(
        "source = \"command\"\ncommand = [\"weather\", \"--quiet\"]\n\
             params = [\"metric\", \"45.25,19.83\"]",
    );
    assert_eq!(argv, ["weather", "--quiet", "metric", "45.25,19.83"]);
}

/// Two modules running the same script with different knobs want two of it, since
/// what is run is what tells one command from another.
#[test]
fn changing_a_param_asks_for_a_different_command() {
    let here = argv_of("source = \"command\"\ncommand = [\"w\"]\nparams = [\"here\"]");
    let there = argv_of("source = \"command\"\ncommand = [\"w\"]\nparams = [\"there\"]");
    assert_ne!(here, there);
}

/// One command asked about three places says three things, and the module scrolls
/// between them.
#[test]
fn a_command_can_publish_a_page_per_line() {
    let cfg = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"weather\"]\ninterval = \"once\"\npages = true",
    ))
    .expect("a command with pages");
    match &cfg.modules().next().unwrap().source {
        Source::Native(Which::Command(spec)) => assert!(spec.pages),
        other => panic!("source is {other:?}"),
    }
}

/// Paging is what a run's lines mean, so two modules that read them differently are
/// two commands rather than one they would have to agree about.
#[test]
fn a_command_is_given_a_deadline_whether_it_asks_for_one_or_not() {
    let cfg = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"w\"]\ninterval = \"1s\"",
    ))
    .expect("a command module");
    let spec = match &cfg.modules().next().unwrap().source {
        Source::Native(Which::Command(spec)) => spec.clone(),
        other => panic!("expected a command, got {other:?}"),
    };
    assert_eq!(spec.timeout, DEFAULT_COMMAND_TIMEOUT);

    let cfg = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"w\"]\ninterval = \"1s\"\ntimeout = \"2s\"",
    ))
    .expect("a command module with a deadline");
    let spec = match &cfg.modules().next().unwrap().source {
        Source::Native(Which::Command(spec)) => spec.clone(),
        other => panic!("expected a command, got {other:?}"),
    };
    assert_eq!(spec.timeout, Duration::from_secs(2));
}

/// The table of running programs is a fixed size, because a signal handler reads it and
/// a handler may not take a lock. A config that could overrun it is refused when it is
/// read: the alternative is a program silently left behind when the bar stops.
///
/// What counts is the collector, not the module. Two modules that name the same command
/// share one collector and so one program, the way two modules reading the processor
/// share one reading.
#[test]
fn a_config_that_runs_more_programs_than_can_be_stopped_is_refused() {
    // `how_many` modules, each running its own program unless `share` is set, in which
    // case they all name the same one.
    let modules = |how_many: usize, share: bool| {
        let names: Vec<String> = (0..how_many).map(|n| format!("run{n}")).collect();
        let mut text = format!("[left]\ngroups = [\"g\"]\n[group.g]\nmodules = {names:?}\n");
        for (n, name) in names.iter().enumerate() {
            let argv = match share {
                true => "\"true\"".to_string(),
                false => format!("\"true\", \"{n}\""),
            };
            text += &format!(
                "[module.{name}]\nsource = \"command\"\ncommand = [{argv}]\nformat = \"$text\"\n"
            );
        }
        text
    };
    let all = crate::proc::AT_ONCE;
    Config::parse(&modules(all, false)).expect("as many programs as dbar can keep track of");
    // Sharing one command is one program, however many modules show it.
    Config::parse(&modules(all + 20, true)).expect("many modules, one program");

    let e = Config::parse(&modules(all + 1, false))
        .expect_err("one more program than dbar can keep track of");
    let message = format!("{e:#}");
    assert!(message.contains(&format!("{all}")), "{message}");
    assert!(message.contains("command modules"), "{message}");
}

/// A streaming command is meant to sit there, so a deadline on one would mean killing
/// a working program for doing its job.""
#[test]
fn a_streaming_command_cannot_be_given_a_deadline() {
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"tail\"]\ntimeout = \"2s\"",
    ))
    .expect_err("a streaming command has nothing to time out");
    let message = format!("{e:#}");
    assert!(message.contains("timeout"), "{message}");
    assert!(message.contains("interval"), "{message}");
}

#[test]
fn a_deadline_on_a_module_that_runs_nothing_is_rejected() {
    let e = Config::parse(&one_module("source = \"cpu\"\ntimeout = \"2s\""))
        .expect_err("only a command has a run to time out");
    let message = format!("{e:#}");
    assert!(message.contains("timeout"), "{message}");
    assert!(message.contains("command"), "{message}");
}

#[test]
fn paging_tells_one_command_from_another() {
    use crate::collect::command::Run;
    let plain = crate::collect::CommandSpec {
        argv: vec!["w".to_string()],
        run: Run::Once,
        pages: false,
        fields: crate::collect::command::PLAIN,
        timeout: DEFAULT_COMMAND_TIMEOUT,
    };
    let paged = crate::collect::CommandSpec {
        pages: true,
        ..plain.clone()
    };
    assert_ne!(plain, paged);
}

#[test]
fn a_streaming_command_has_no_lines_to_page_between() {
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"tail\"]\npages = true",
    ))
    .expect_err("a streaming command sends a reading per line as it goes");
    let message = format!("{e:#}");
    assert!(message.contains("interval"), "{message}");
}

#[test]
fn pages_belong_to_a_command_and_nothing_else() {
    let e = Config::parse(&one_module("source = \"cpu\"\npages = true"))
        .expect_err("a cpu module publishes one reading");
    let message = format!("{e:#}");
    assert!(message.contains("pages"), "{message}");
}

/// `scroll` is for what dbar can set as well as read. A command module is scrolled
/// too, but through its pages, so the error says which key that is.
#[test]
fn scrolling_a_command_points_at_pages() {
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"w\"]\nscroll = \"5%\"",
    ))
    .expect_err("a command is not something dbar can set");
    let message = format!("{e:#}");
    assert!(message.contains("pages = true"), "{message}");
}

#[test]
fn params_belong_to_a_command_and_nothing_else() {
    let e = Config::parse(&one_module("source = \"cpu\"\nparams = [\"metric\"]"))
        .expect_err("a cpu module runs nothing to pass them to");
    let message = format!("{e:#}");
    assert!(message.contains("params"), "{message}");
    assert!(message.contains("command"), "{message}");
}

/// A reading that costs a request to somebody else's server is worth asking for
/// rather than taking on a schedule, and this is what asks.
#[test]
fn a_button_can_ask_a_source_to_be_read_again() {
    let cfg = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"weather\"]\ninterval = \"once\"\n\
             refresh_button = \"left\"",
    ))
    .expect("a command that answers when asked");
    let module = cfg.modules().next().expect("the one module");
    assert_eq!(module.refresh_button, Some(Button::Left));
    assert_eq!(cfg.refreshable().len(), 1, "something can ask for it");
}

/// A command nothing can ask keeps no way of being asked, so one that answers once is
/// done when it has answered rather than parking a thread on a question that cannot
/// come.
#[test]
fn a_command_nobody_can_ask_is_not_askable() {
    let cfg = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"uname\"]\ninterval = \"once\"",
    ))
    .expect("a command module");
    assert!(cfg.refreshable().is_empty());
}

#[test]
fn a_streaming_command_has_no_run_to_bring_forward() {
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"tail\"]\nrefresh_button = \"left\"",
    ))
    .expect_err("a streaming command speaks for itself");
    let message = format!("{e:#}");
    assert!(message.contains("interval"), "{message}");
}

/// The volume arrives when it moves. Asking for it would be asking a collector that
/// exists only to say it cannot answer, so the config says so instead.
#[test]
fn a_source_that_arrives_on_its_own_cannot_be_asked() {
    for key in ["refresh_button = \"left\"", "signal = 3"] {
        let e = Config::parse(&one_module(&format!("source = \"audio\"\n{key}")))
            .expect_err("the volume is not read");
        let message = format!("{e:#}");
        assert!(message.contains("arrives when it changes"), "{message}");
    }
}

#[test]
fn refreshing_cannot_take_a_button_something_else_has() {
    let e = Config::parse(&one_module(
        "source = \"command\"\ncommand = [\"weather\"]\ninterval = \"once\"\n\
             format_alt = \"$text!\"\nrefresh_button = \"left\"",
    ))
    .expect_err("the left button was already turning the page");
    let message = format!("{e:#}");
    assert!(message.contains("format_alt"), "{message}");
    assert!(message.contains("refresh_button"), "{message}");
}

#[test]
fn a_click_can_run_a_program_of_your_own() {
    let cfg = Config::parse(&one_module(
        "source = \"time\"\non_click = { left = [\"cal\", \"-3\"] }",
    ))
    .expect("a clock with something to run");
    let module = cfg.modules().next().expect("the one module");
    let actions = module.on_click.as_ref().expect("on_click was written");
    assert_eq!(
        actions.for_button(Button::Left),
        Some(["cal".to_string(), "-3".to_string()].as_slice())
    );
    assert_eq!(actions.for_button(Button::Right), None);
}

#[test]
fn a_button_given_nothing_to_run_is_a_mistake_worth_stopping_for() {
    let e = Config::parse(&one_module(
        "source = \"time\"\non_click = { left = [\"\"] }",
    ))
    .expect_err("an empty program name runs nothing");
    let message = format!("{e:#}");
    assert!(message.contains("on_click.left"), "{message}");
    assert!(message.contains("[module.m]"), "{message}");
}

#[test]
fn further_wordings_answer_to_the_left_button_unless_told_otherwise() {
    let cfg = Config::parse(&one_module(
        "source = \"cpu\"\nformat_alt = \"$utilization\"",
    ))
    .expect("a module with a second wording");
    assert_eq!(cfg.modules().next().unwrap().alt_button, Button::Left);

    let moved = Config::parse(&one_module(
        "source = \"cpu\"\nformat_alt = \"$utilization\"\nalt_button = \"middle\"",
    ))
    .expect("the button is the config's to choose");
    assert_eq!(moved.modules().next().unwrap().alt_button, Button::Middle);
}

#[test]
fn two_things_wanting_one_button_is_a_startup_error() {
    // The left button cannot both run a program and move through wordings; picking a
    // winner silently would leave the loser spelled correctly and doing nothing.
    let e = Config::parse(&one_module(
        "source = \"cpu\"\nformat_alt = \"$utilization\"\non_click = { left = [\"true\"] }",
    ))
    .expect_err("the left button was given away twice");
    let message = format!("{e:#}");
    assert!(message.contains("left"), "{message}");
    assert!(message.contains("format_alt"), "{message}");
    assert!(message.contains("on_click.left"), "{message}");

    // Moving one of them off it settles the matter.
    Config::parse(&one_module(
            "source = \"cpu\"\nformat_alt = \"$utilization\"\nalt_button = \"middle\"\non_click = { left = [\"true\"] }",
        ))
        .expect("nothing is claimed twice now");
}

#[test]
fn folding_answers_to_the_right_button_unless_told_otherwise() {
    let cfg = Config::parse(&one_module(
        "source = \"cpu\"\ncollapsible = true\nicon = \"$cpu\"",
    ))
    .expect("a module that folds");
    assert_eq!(cfg.modules().next().unwrap().collapse_button, Button::Right);

    // Moved off the right, which then leaves the right free for something else.
    let moved = Config::parse(&one_module(
            "source = \"cpu\"\ncollapsible = true\nicon = \"$cpu\"\ncollapse_button = \"middle\"\non_click = { right = [\"true\"] }",
        ))
        .expect("nothing is claimed twice");
    let module = moved.modules().next().unwrap();
    assert_eq!(module.collapse_button, Button::Middle);
    assert!(module.on_click.is_some());
}

#[test]
fn muting_answers_to_the_middle_button_unless_told_otherwise() {
    let cfg =
        Config::parse(&one_module("source = \"audio\"\nscroll = \"5%\"")).expect("a volume module");
    assert_eq!(
        cfg.modules().next().unwrap().mute_button,
        Some(Button::Middle)
    );

    // Moved onto the left, with the wordings moved off it so nothing is claimed twice.
    let moved = Config::parse(&one_module(
            "source = \"audio\"\nscroll = \"5%\"\nmute_button = \"left\"\nformat_alt = [\" $volume \"]\nalt_button = \"right\"",
        ))
        .expect("nothing is claimed twice");
    assert_eq!(
        moved.modules().next().unwrap().mute_button,
        Some(Button::Left)
    );

    // A module with nothing to mute carries no button for it, so the press falls
    // through to whatever the module was forwarding to.
    let cpu = Config::parse(&one_module("source = \"cpu\"")).expect("a cpu module");
    assert_eq!(cpu.modules().next().unwrap().mute_button, None);
}

#[test]
fn muting_is_refused_where_there_is_nothing_to_mute() {
    let e = Config::parse(&one_module("source = \"cpu\"\nmute_button = \"left\""))
        .expect_err("a cpu module cannot be muted");
    assert!(format!("{e:#}").contains("mute_button"), "{e:#}");
}

#[test]
fn the_button_that_mutes_is_claimed_like_any_other() {
    // The volume's own button is taken, so a program cannot be given the same one.
    let e = Config::parse(&one_module(
        "source = \"audio\"\nscroll = \"5%\"\non_click = { middle = [\"true\"] }",
    ))
    .expect_err("muting is already on the middle");
    assert!(format!("{e:#}").contains("mute_button"), "{e:#}");

    // Moving the mute frees the button it left behind.
    Config::parse(&one_module(
            "source = \"audio\"\nscroll = \"5%\"\nmute_button = \"right\"\non_click = { middle = [\"true\"] }",
        ))
        .expect("the middle is free once muting has moved off it");
}

#[test]
fn a_button_a_native_control_already_uses_is_claimed_too() {
    // `controls` on a player is the left button, and a right click folds any module
    // down, so neither is free to be given away a second time.
    let e = Config::parse(&one_module(
        "source = \"media\"\ncontrols = true\non_click = { left = [\"true\"] }",
    ))
    .expect_err("play and pause are already on the left");
    assert!(format!("{e:#}").contains("controls"), "{e:#}");

    let e = Config::parse(&one_module(
        "source = \"cpu\"\ncollapsible = true\non_click = { right = [\"true\"] }",
    ))
    .expect_err("folding is already on the right");
    assert!(format!("{e:#}").contains("collapsible"), "{e:#}");
}

use super::*;

#[test]
fn the_built_in_default_config_parses() {
    Config::parse(DEFAULT_CONFIG).expect("the compiled-in default must parse");
}

#[test]
fn a_format_naming_an_unknown_field_is_rejected() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
format = "$nope"
"##;
    let e = Config::parse(config).expect_err("an unknown field must be reported");
    let message = format!("{e:#}");
    assert!(message.contains("[module.cpu]"), "{message}");
    assert!(message.contains("$nope"), "{message}");
}

#[test]
fn a_format_is_checked_against_the_source_it_reads() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["win"]

[module.win]
source = "sway:window"
format = "$text"
"##;
    // `$text` is the provider's field; the window module publishes `$title`.
    let e = Config::parse(config).expect_err("the wrong source's field must be reported");
    assert!(format!("{e:#}").contains("title"), "{e:#}");
}

#[test]
fn a_layout_mapping_belongs_to_the_module_that_reads_it() {
    let config = |source: &str| {
        format!(
            r##"
[left]
groups = ["g"]

[group.g]
modules = ["lang"]

[module.lang]
source = "{source}"

[module.lang.layouts]
"English (US)" = "EN"
"##
        )
    };

    let parsed = Config::parse(&config("sway:language")).expect("parses");
    assert!(parsed.needs_language());
    let Source::SwayLanguage(layouts) = &parsed.modules().next().expect("one module").source else {
        panic!("the module should read the compositor's keyboard layout");
    };
    assert_eq!(layouts["English (US)"], "EN");

    // On anything else the table would quietly do nothing, which is how a config comes
    // to be wrong for months.
    let e = Config::parse(&config("cpu")).expect_err("a misplaced mapping must be reported");
    assert!(format!("{e:#}").contains("layouts"), "{e:#}");
}

#[test]
fn a_workspace_icon_must_name_a_native_icon() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws"]

[module.ws]
source = "sway:workspaces"
icons = { "1" = "$not-an-icon" }
"##;
    let error = format!(
        "{:#}",
        Config::parse(config).expect_err("an unknown workspace icon must be rejected")
    );
    assert!(error.contains("workspace \"1\""), "{error}");
    assert!(
        error.contains("unknown native icon \"$not-an-icon\""),
        "{error}"
    );
}

#[test]
fn a_bar_without_a_language_module_never_asks_for_one() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["win"]

[module.win]
source = "sway:window"
"##;
    assert!(!Config::parse(config).expect("parses").needs_language());
}

#[test]
fn durations_need_a_unit() {
    assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
    assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
    assert_eq!(parse_duration(" 1h ").unwrap(), Duration::from_secs(3600));
    assert_eq!(parse_duration("0.5s").unwrap(), Duration::from_millis(500));

    // "2" reads as two of something, and which something is the point.
    assert!(parse_duration("2").is_err());
    assert!(parse_duration("2w").is_err());
    assert!(parse_duration("0s").is_err());
    assert!(parse_duration("-1s").is_err());
    assert!(parse_duration("s").is_err());
}

/// A flag is compared against the words it is drawn with, so a source that publishes
/// one where it used to publish text does not silence every config that matched on the
/// word. The audio module's `muted` is exactly that: it was `"yes"` and `"no"` as text
/// before it was the flag it always was.
#[test]
fn a_state_rule_matches_a_flag_by_the_word_it_is_drawn_with() {
    use crate::status::Value;

    let config = |compare: &str| {
        format!(
            r##"
[left]
groups = ["g"]
[group.g]
modules = ["vol"]
[module.vol]
source = "audio"
format = "$volume"
[module.vol.states.quiet]
{compare}
foreground = "#ff0000"
"##
        )
    };
    // Both ways of writing the rule are accepted against a field that is a flag.
    for compare in [
        "field = \"muted\"\nequals = \"yes\"",
        "fields = { muted = \"yes\" }",
        "field = \"muted\"\nequals = \"true\"",
    ] {
        Config::parse(&config(compare))
            .unwrap_or_else(|e| panic!("{compare:?} should be a rule about a flag: {e:#}"));
    }
    // And a number is still not a word.
    assert!(Config::parse(&config("field = \"volume\"\nequals = \"yes\"")).is_err());

    // What the matching itself does, which is what a config written against the old
    // text depends on.
    let yes = Value::Flag(true);
    let no = Value::Flag(false);
    assert!(yes.reads_as("yes") && yes.reads_as("YES") && yes.reads_as("true"));
    assert!(no.reads_as("no") && no.reads_as("false"));
    assert!(!yes.reads_as("no") && !no.reads_as("yes"));
    // Text goes on reading as itself.
    assert!(Value::Text("headphones".into()).reads_as("HeadPhones"));
}

/// A length is refused at both ends rather than rounded into something that looks
/// like an answer. Too small used to arrive as zero, which leaves a collector due the
/// moment it has been read - a bar reading a source as fast as the machine can, which
/// is what an interval exists to prevent. Too large used to panic in the conversion,
/// so a config nobody could run took `--check-config` down rather than being reported.
#[test]
fn a_length_of_time_has_two_ends() {
    assert_eq!(parse_duration("1ms").unwrap(), Duration::from_millis(1));

    for shorter in ["0.0000000001s", "0.0001ms", "0.00000001m"] {
        let e = parse_duration(shorter).expect_err("shorter than dbar can schedule");
        assert!(format!("{e:#}").contains("shorter"), "{e:#}");
    }
    // Scientific notation is refused a step earlier - "e300s" is not a unit - so the
    // cases here are the ones that reach the conversion as a number.
    //
    // The last two fit in a `Duration` and are still not schedules: a deadline is an
    // `Instant` plus the wait, and a wait that keeps failing is doubled, and both of
    // those panic on overflow rather than saturating. A bar that started would stop
    // the first time that collector came due.
    for longer in [
        "999999999999999999999999999s",
        "99999999999999999999h",
        "9223372036854775808s",
        "1000000000000000000s",
    ] {
        let e = parse_duration(longer).expect_err("longer than dbar can schedule");
        assert!(format!("{e:#}").contains("longer"), "{e:#}");
    }

    // A year is allowed, and survives everything scheduling does to it.
    let year = parse_duration("8760h").expect("a year is a length of time");
    let latest = crate::collect::backoff(year, u32::MAX);
    assert!(
        std::time::Instant::now().checked_add(latest).is_some(),
        "the longest allowed interval does not survive being backed off"
    );
}

#[test]
fn a_collector_is_read_once_however_many_modules_show_it() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["a", "b"]

[module.a]
source = "cpu"
interval = "5s"

[module.b]
source = "cpu"
interval = "1s"
"##;
    let collectors = Config::parse(config).expect("parses").collectors();
    assert_eq!(collectors.len(), 1);
    // The shortest interval anyone asked for wins, so nobody waits longer than they said.
    assert_eq!(collectors[&Which::Cpu], Duration::from_secs(1));
}

#[test]
fn a_native_config_needs_no_provider() {
    let native = r##"
[left]
groups = ["g"]

[group.g]
modules = ["clock"]

[module.clock]
source = "time"
"##;
    assert!(!Config::parse(native).expect("parses").needs_provider());
    // The built-in default reads everything itself, so it starts nothing.
    assert!(
        !Config::parse(DEFAULT_CONFIG)
            .expect("parses")
            .needs_provider()
    );

    let external = r##"
[left]
groups = ["g"]

[group.g]
modules = ["net"]
"##;
    assert!(Config::parse(external).expect("parses").needs_provider());
}

#[test]
fn an_interval_on_a_source_dbar_does_not_read_is_rejected() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
interval = "1s"
"##;
    let e = Config::parse(config).expect_err("an interval on a provider module is a mistake");
    assert!(format!("{e:#}").contains("interval"), "{e:#}");
}

#[test]
fn an_unknown_source_says_what_the_sources_are() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["x"]

[module.x]
source = "nonesuch"
"##;
    let e = Config::parse(config).expect_err("an unknown source is a mistake");
    let message = format!("{e:#}");
    assert!(message.contains("nonesuch"), "{message}");
    assert!(message.contains("cpu"), "{message}");
}

#[test]
fn matching_on_text_is_only_for_provider_modules() {
    let native = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"

[module.cpu.states.busy]
contains = "busy"
"##;
    let e = Config::parse(native).expect_err("a native source publishes values, not wording");
    assert!(format!("{e:#}").contains("text"), "{e:#}");

    // The same rule is exactly right for a module fed rendered text.
    let provider = native.replace("source = \"cpu\"\n", "");
    assert!(Config::parse(&provider).is_ok());
}

#[test]
fn a_threshold_field_has_to_be_a_number_the_source_publishes() {
    let template = r##"
[left]
groups = ["g"]

[group.g]
modules = ["mem"]

[module.mem]
source = "memory"

[module.mem.states.rule]
field = "FIELD"
above = 10
"##;
    assert!(Config::parse(&template.replace("FIELD", "swap_percent")).is_ok());

    let unknown = Config::parse(&template.replace("FIELD", "nonesuch"))
        .expect_err("a field the source does not publish is a mistake");
    assert!(format!("{unknown:#}").contains("nonesuch"), "{unknown:#}");

    let wrong_kind = r##"
[left]
groups = ["g"]

[group.g]
modules = ["clock"]

[module.clock]
source = "time"

[module.clock.states.rule]
field = "now"
above = 10
"##;
    let e = Config::parse(wrong_kind).expect_err("a bound on a time could never fire");
    assert!(format!("{e:#}").contains("number"), "{e:#}");
}

#[test]
fn a_rule_comparing_a_word_needs_a_field_that_holds_one() {
    let template = r##"
[left]
groups = ["g"]

[group.g]
modules = ["bat"]

[module.bat]
source = "battery"

[module.bat.states.rule]
field = "FIELD"
COMPARE
"##;
    let ok = template
        .replace("FIELD", "status")
        .replace("COMPARE", "equals = \"charging\"");
    assert!(Config::parse(&ok).is_ok());

    // A word against a number, and a number against a word, are both mistakes.
    let wrong_kind = template
        .replace("FIELD", "percent")
        .replace("COMPARE", "equals = \"charging\"");
    let e = Config::parse(&wrong_kind).expect_err("percent holds no word");
    assert!(format!("{e:#}").contains("word"), "{e:#}");

    let wrong_bound = template
        .replace("FIELD", "status")
        .replace("COMPARE", "above = 10");
    let e = Config::parse(&wrong_bound).expect_err("status holds no number");
    assert!(format!("{e:#}").contains("number"), "{e:#}");

    // Naming a field and then saying nothing about it can never fire.
    let silent = template.replace("FIELD", "status").replace("COMPARE", "");
    let e = Config::parse(&silent).expect_err("a rule with no comparison is a mistake");
    assert!(format!("{e:#}").contains("equals"), "{e:#}");
}

#[test]
fn state_names_are_the_ones_a_source_can_report() {
    assert_eq!(parse_state("critical").unwrap(), State::Critical);
    assert_eq!(parse_state("error").unwrap(), State::Error);
    let e = parse_state("urgent").expect_err("urgent is a flag, not a rating");
    assert!(format!("{e:#}").contains("warning"), "{e:#}");
}

#[test]
fn a_second_wording_is_checked_like_the_first() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
format_alt = "$nonesuch"
"##;
    let e = Config::parse(config).expect_err("format_alt is checked too");
    let message = format!("{e:#}");
    assert!(message.contains("format_alt"), "{message}");
    assert!(message.contains("nonesuch"), "{message}");
}

#[test]
fn a_module_may_have_one_further_wording_or_several() {
    let config = Config::parse(
        r#"
[right]
groups = ["g"]

[group.g]
modules = ["one", "several"]

[module.one]
source = "cpu"
format_alt = " $utilization "

[module.several]
source = "network"
format_alt = [" $down ", " $signal ", " $dbm "]
"#,
    )
    .expect("both spellings are allowed");
    let wordings = |name: &str| {
        config
            .modules()
            .find(|m| m.name == name)
            .expect("the module is there")
            .format_alt
            .len()
    };
    assert_eq!(wordings("one"), 1);
    assert_eq!(wordings("several"), 3);
}

#[test]
fn a_wording_that_names_a_field_the_source_lacks_says_which_one() {
    let broken = Config::parse(
        r#"
[right]
groups = ["g"]

[group.g]
modules = ["net"]

[module.net]
source = "network"
format_alt = [" $down ", " $nonsense "]
"#,
    )
    .expect_err("the second wording names nothing");
    let message = format!("{:#}", broken);
    assert!(message.contains("wording 2"), "{message}");
    assert!(message.contains("nonsense"), "{message}");
}

#[test]
fn folding_needs_something_left_to_click_on() {
    let broken = Config::parse(
        r#"
[right]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
collapsible = true
"#,
    )
    .expect_err("a module with no icon has nothing to fold down to");
    let message = format!("{broken:#}");
    assert!(message.contains("icon"), "{message}");

    Config::parse(
        r#"
[right]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
icon = "$cpu"
collapsible = true
"#,
    )
    .expect("with an icon it is fine");
}

#[test]
fn only_what_dbar_can_set_may_be_scrolled() {
    let broken = r#"
[right]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
scroll = "5%"
"#;
    let message = Config::parse(broken)
        .expect_err("cpu cannot be scrolled")
        .to_string();
    assert!(message.contains("cpu"), "{message}");

    let fine = r#"
[right]
groups = ["g"]

[group.g]
modules = ["light"]

[module.light]
source = "backlight"
scroll = "5%"
"#;
    let config = Config::parse(fine).expect("a backlight can be scrolled");
    let module = config
        .modules()
        .find(|m| m.name == "light")
        .expect("the module is there");
    assert_eq!(module.control, Some((Control::Brightness, 5.0)));
}

/// A bar with nothing to say about screens goes on all of them, including one plugged
/// in later, which is what a session with one monitor has always had.
#[test]
fn a_bar_that_names_no_screens_goes_on_every_one() {
    let bar = Config::parse("[bar]\nheight = 20\n").expect("parses").bar;
    assert!(bar.shows_on(Some("DP-1")));
    assert!(bar.shows_on(None));
}

#[test]
fn a_bar_that_names_screens_goes_only_on_those() {
    let bar = Config::parse("[bar]\noutputs = [\"DP-1\", \"DP-2\"]\n")
        .expect("parses")
        .bar;
    assert!(bar.shows_on(Some("DP-1")));
    assert!(!bar.shows_on(Some("HDMI-A-1")));
    // A screen the compositor has not named yet cannot be one of two written down.
    assert!(!bar.shows_on(None));

    let all = Config::parse("[bar]\noutputs = [\"*\"]\n")
        .expect("parses")
        .bar;
    assert!(all.shows_on(Some("HDMI-A-1")));
    assert!(all.shows_on(None));
}

/// `scope` is about what is on a screen, so it means nothing on a module that is not
/// drawn from the compositor - and a key that quietly does nothing is how a config
/// comes to be wrong for months.
#[test]
fn scope_on_a_module_that_is_not_about_a_screen_is_rejected() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
source = "cpu"
scope = "session"
"##;
    let e = Config::parse(config).expect_err("scope means nothing to a cpu module");
    let message = format!("{e:#}");
    assert!(message.contains("scope"), "{message}");
    assert!(message.contains("sway:window"), "{message}");
}

#[test]
fn a_compositor_module_is_about_its_own_screen_unless_it_says_otherwise() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["ws", "win"]

[module.ws]
source = "sway:workspaces"

[module.win]
source = "sway:window"
scope = "session"
"##;
    let cfg = Config::parse(config).expect("parses");
    let source = |name: &str| {
        cfg.modules()
            .find(|m| m.name == name)
            .expect("the module is there")
            .source
            .clone()
    };
    assert_eq!(
        source("ws"),
        Source::SwayWorkspaces(WorkspaceView::default())
    );
    assert_eq!(source("win"), Source::SwayWindow(Scope::Session));
}

#[test]
fn a_scroll_step_is_a_percentage_or_an_error_saying_so() {
    for written in ["0%", "101%", "some"] {
        assert!(parse_percent(written).is_err(), "{written} was accepted");
    }
    assert_eq!(parse_percent("5%").ok(), Some(5.0));
    assert_eq!(parse_percent(" 2.5 ").ok(), Some(2.5));
}

#[test]
fn a_signal_names_the_sources_it_reads_again() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["light", "light2", "cpu"]

[module.light]
source = "backlight"
signal = 8

[module.light2]
source = "backlight"
signal = 8

[module.cpu]
source = "cpu"
signal = 9
"##;
    let signals = Config::parse(config).expect("parses").signals();
    // Two modules on one source and one signal is still one source to read.
    assert_eq!(signals[&8], vec![Which::Backlight]);
    assert_eq!(signals[&9], vec![Which::Cpu]);
}

#[test]
fn a_signal_outside_the_realtime_range_is_rejected() {
    let template = r##"
[left]
groups = ["g"]

[group.g]
modules = ["light"]

[module.light]
source = "backlight"
signal = N
"##;
    assert!(Config::parse(&template.replace("N", "0")).is_ok());
    assert!(Config::parse(&template.replace("N", "-1")).is_err());
    let too_high = (signal_range() + 1).to_string();
    let e = Config::parse(&template.replace("N", &too_high))
        .expect_err("a signal this system does not have is a mistake");
    assert!(format!("{e:#}").contains("SIGRTMIN"), "{e:#}");
}

#[test]
fn a_signal_on_a_source_dbar_does_not_read_is_rejected() {
    let config = r##"
[left]
groups = ["g"]

[group.g]
modules = ["cpu"]

[module.cpu]
signal = 8
"##;
    let e = Config::parse(config).expect_err("a provider handles its own signals");
    assert!(format!("{e:#}").contains("signal"), "{e:#}");
}

#[test]
fn the_bar_sits_above_ordinary_windows_unless_told_otherwise() {
    assert_eq!(Config::parse("").unwrap().bar.layer, BarLayer::Top);
    let config = Config::parse("[bar]\nlayer = \"bottom\"\n").unwrap();
    assert_eq!(config.bar.layer, BarLayer::Bottom);
    let e = Config::parse("[bar]\nlayer = \"above\"\n").expect_err("not a layer");
    assert!(format!("{e:#}").contains("layer"), "{e:#}");
}

#[test]
fn an_island_is_all_there_unless_it_asks_not_to_be() {
    let group = |line: &str| {
        format!(
            "[right]\ngroups = [\"system\"]\n\
                 [group.system]\nmodules = [\"cpu\"]\n{line}\n\
                 [module.cpu]\nsource = \"cpu\"\n"
        )
    };
    let opacity = |toml: &str| {
        Config::parse(toml).map(|c| {
            c.positions
                .iter()
                .flat_map(|p| &p.groups)
                .next()
                .expect("the group was placed")
                .opacity
        })
    };

    assert_eq!(opacity(&group("")).expect("a group needs no opacity"), 1.0);
    assert_eq!(opacity(&group("opacity = 0.5")).expect("half is fine"), 0.5);

    // Named, and pointing at the group, so the mistake is findable at startup rather
    // than at three in the morning.
    for bad in ["opacity = 1.8", "opacity = -0.2"] {
        let e = opacity(&group(bad)).expect_err("outside 0.0 to 1.0");
        let message = format!("{e:#}");
        assert!(message.contains("opacity"), "{message}");
        assert!(message.contains("group.system"), "{message}");
    }
}

/// A state that is a combination of readings has to beat the rules that name either
/// half, or muted headphones would show whichever of the two got sorted first.
#[test]
fn a_rule_naming_two_readings_beats_one_naming_either() {
    let config = r##"
[right]
groups = ["g"]

[group.g]
modules = ["volume"]

[module.volume]
source = "audio"
icon = "$volume"

[module.volume.states.zz_both]
fields = { muted = "yes", port = "headphones" }
icon = "$headphones-muted"

[module.volume.states.muted]
field = "muted"
equals = "yes"
icon = "$volume-muted"

[module.volume.states.port]
field = "port"
equals = "headphones"
icon = "$headphones"
"##;
    let cfg = Config::parse(config).expect("parses");
    let module = cfg
        .positions
        .iter()
        .flat_map(|p| &p.groups)
        .next()
        .unwrap()
        .modules[0]
        .clone();

    let says = |muted: &str, port: &str| {
        let mut fields = crate::status::Fields::default();
        fields.set("muted", crate::status::Value::Text(muted.to_string()));
        fields.set("port", crate::status::Value::Text(port.to_string()));
        module
            .states
            .iter()
            .find(|rule| rule.matches(StateFlags::default(), false, &fields, ""))
            .and_then(|rule| rule.style.icon.clone())
    };

    assert_eq!(
        says("yes", "headphones"),
        Icon::parse("headphones-muted").map(IconSpec::Native)
    );
    assert_eq!(
        says("yes", "speaker"),
        Icon::parse("volume-muted").map(IconSpec::Native)
    );
    assert_eq!(
        says("no", "headphones"),
        Icon::parse("headphones").map(IconSpec::Native)
    );
    assert_eq!(
        says("no", "speaker"),
        None,
        "nothing unusual is being reported"
    );
}

/// A combined rule is checked against the source the same way a single one is.
#[test]
fn a_combined_rule_cannot_name_a_field_the_source_does_not_publish() {
    let config = r##"
[right]
groups = ["g"]

[group.g]
modules = ["volume"]

[module.volume]
source = "audio"

[module.volume.states.odd]
fields = { muted = "yes", jack = "in" }
"##;
    let e = Config::parse(config).expect_err("jack is not an audio field");
    let message = format!("{e:#}");
    assert!(message.contains("$jack"), "{message}");
}

#[test]
fn every_shipped_example_parses() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    for entry in std::fs::read_dir(dir).expect("examples/ is readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|e| e != "toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable example");
        if let Err(e) = Config::parse(&text) {
            panic!("{} does not parse: {e:#}", path.display());
        }
    }
}
#[test]
fn group_joins_are_opt_in_and_validate_their_groups() {
    let config = |separator: &str, group: &str| {
        format!(
            r#"
[right]
groups = ["g"]
{separator}
[group.g]
modules = ["cpu"]
{group}
[module.cpu]
source = "cpu"
"#
        )
    };
    for separator in ["", "[right.separator]\nshape = 'none'"] {
        let cfg = Config::parse(&config(separator, "opacity = 0.5\npadding = 3")).unwrap();
        assert!(cfg.positions[2].separator.is_none());
        assert_eq!(cfg.positions[2].groups[0].opacity, 0.5);
    }
    let joined = "[right.separator]\nshape = 'slant'\nwidth = 6";
    let cfg = Config::parse(&config(joined, "")).unwrap();
    assert_eq!(cfg.positions[2].separator.unwrap().width, 6.0);
    assert!(cfg.positions[..2].iter().all(|p| p.separator.is_none()));
    for group in ["opacity = 0.5", "padding = 3"] {
        let error = Config::parse(&config(joined, group))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("right.separator") && error.contains("group \"g\""),
            "{error}"
        );
    }
    for bad in [
        "width = 0",
        "width = -1",
        "width = inf",
        "overlap = inf",
        "color = '$missing'",
        "shape = 'unknown'",
    ] {
        let separator = format!(
            "[right.separator]\n{}\n{bad}",
            if bad.starts_with("shape") {
                ""
            } else {
                "shape = 'slant'"
            }
        );
        assert!(Config::parse(&config(&separator, "")).is_err(), "{bad}");
    }
}

/// A group answers for its whole island, so a button it reserves never reaches the
/// modules inside it. dbar refuses to let a module hand one button two jobs; a group
/// taking a button a module is already using is the same mistake one level up, and the
/// module's key would be there, spelled correctly, and dead.
#[test]
fn a_group_may_not_reserve_a_button_one_of_its_modules_uses() {
    let config = |group: &str, module: &str| {
        format!(
            "[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\ncollapsible = true\n\
                 collapse_button = '{group}'\ncollapsed = {{ icon = '$cpu' }}\n\
                 [module.m]\nsource = 'cpu'\n{module}\n"
        )
    };

    for (button, module, by) in [
        (
            "right",
            "collapsible = true
icon = '$cpu'",
            "collapsible",
        ),
        ("left", "format_alt = '$utilization'", "format_alt"),
        ("middle", "refresh_button = 'middle'", "refresh_button"),
        ("left", "on_click = { left = ['true'] }", "on_click.left"),
    ] {
        let e = Config::parse(&config(button, module))
            .expect_err("the group and the module both want that button");
        let message = format!("{e:#}");
        assert!(message.contains(by), "{message}");
        assert!(message.contains("\"m\""), "{message}");
        assert!(message.contains(button), "{message}");
    }

    // A module operating the volume claims the button that mutes it.
    let volume = "[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\ncollapsible = true\n\
             collapse_button = 'middle'\ncollapsed = { icon = '$cpu' }\n\
             [module.m]\nsource = 'audio'\nscroll = '5%'\n";
    let e = Config::parse(volume).expect_err("middle mutes");
    assert!(format!("{e:#}").contains("mute_button"), "{e:#}");

    // And a module that leaves the reserved button alone is fine, however much else
    // it does with the other two.
    Config::parse(&config(
        "right",
        "format_alt = '$utilization'\nrefresh_button = 'middle'",
    ))
    .expect("nothing here wants the right button");
}

#[test]
fn a_fold_is_instant_unless_the_config_gives_it_a_time() {
    let prefix = "[left]\ngroups = ['system']\n[group.system]\nmodules = []\n";
    let parse = |extra: &str| Config::parse(&format!("{prefix}{extra}"));
    let shut = "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = '$cpu' }";
    let collapse = |cfg: &Config| cfg.positions[0].groups[0].collapse.clone().unwrap();

    // The default is the behaviour that costs nothing: a fold is one redraw.
    assert!(collapse(&parse(shut).unwrap()).animation.is_none());
    assert_eq!(
        collapse(&parse(&format!("{shut}\ncollapse_animation = '150ms'")).unwrap()).animation,
        Some(Duration::from_millis(150))
    );
    for (extra, message) in [
        (format!("{shut}\ncollapse_animation = '150'"), "unit"),
        (
            "collapse_animation = '150ms'".to_string(),
            "without collapsible",
        ),
        // A fold redraws at screen rate for the whole of its span, so a long one is
        // not a slower animation - it is the permanent tick the bar exists without.
        (format!("{shut}\ncollapse_animation = '15s'"), "longer than"),
        (format!("{shut}\ncollapse_animation = '1h'"), "longer than"),
    ] {
        let error = format!("{:#}", parse(&extra).unwrap_err());
        assert!(
            error.contains(message),
            "{error:?} should mention {message}"
        );
    }
}

/// A wording swaps in one redraw unless the config gives it a span to travel over,
/// and a span belongs to a module that has somewhere to travel to.
#[test]
fn a_wording_swap_is_instant_unless_the_config_gives_it_a_time() {
    let prefix = "[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\n[module.m]\n\
                      source = 'cpu'\n";
    let parse = |extra: &str| Config::parse(&format!("{prefix}{extra}"));
    let module = |cfg: &Config| cfg.positions[0].groups[0].modules[0].clone();
    let alt = "format_alt = '$utilization'";

    assert!(module(&parse(alt).unwrap()).alt_animation.is_none());
    assert_eq!(
        module(&parse(&format!("{alt}\nalt_animation = '120ms'")).unwrap()).alt_animation,
        Some(Duration::from_millis(120))
    );
    for (extra, message) in [
        (format!("{alt}\nalt_animation = '120'"), "unit"),
        // Nowhere to travel to, so the key is one that would be spelled correctly and
        // do nothing at all.
        ("alt_animation = '120ms'".to_string(), "without format_alt"),
        // The same ceiling a fold answers to: past it this is not a slower travel, it
        // is the permanent tick the bar exists without.
        (format!("{alt}\nalt_animation = '15s'"), "longer than"),
    ] {
        let error = format!("{:#}", parse(&extra).unwrap_err());
        assert!(
            error.contains(message),
            "{error:?} should mention {message}"
        );
    }
}

#[test]
fn group_collapse_defaults_requirements_and_style_cascade() {
    let prefix = "[left]\ngroups = ['system']\n[group.system]\nmodules = []\n";
    let parse = |extra: &str| Config::parse(&format!("{prefix}{extra}"));
    let cfg = parse("").unwrap();
    assert_eq!(cfg.positions[0].groups[0].name, "system");
    assert!(cfg.positions[0].groups[0].collapse.is_none());
    for (extra, message) in [
        ("collapsible = true", "collapse_button"),
        ("collapsible = true\ncollapse_button = 'right'", "icon"),
        ("collapsible = true\ncollapse_button = 'wheel'", "wheel"),
        (
            "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = '$bogus' }",
            "unknown native icon",
        ),
        (
            "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = 'none' }",
            "icon",
        ),
        (
            "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = '$cpu', icon_size = 0 }",
            "icon_size",
        ),
        (
            "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = '$cpu', icon_size = -1 }",
            "icon_size",
        ),
        (
            "collapsible = true\ncollapse_button = 'right'\ncollapsed = { icon = '$cpu', icon_size = inf }",
            "icon_size",
        ),
        (
            "collapsible = true\ncollapse_button = 'right'\ncollapsed = { style = 'missing' }",
            "unknown style",
        ),
        ("collapsed = { typo = 1 }", "unknown field"),
    ] {
        let error = format!("{:#}", parse(extra).unwrap_err());
        assert!(error.contains(message), "{error}");
    }
    for button in ["left", "middle", "right"] {
        let cfg = parse(&format!(
            "collapsible = true\ncollapse_button = '{button}'\ncollapsed = {{ icon = '$cpu' }}"
        ))
        .unwrap();
        let collapse = cfg.positions[0].groups[0].collapse.as_ref().unwrap();
        assert_eq!(collapse.button.name(), button);
        assert_eq!(collapse.style.padding, Style::default().padding);
        assert_eq!(collapse.style.icon_size, cfg.bar.icon_size);
    }
    let cfg = parse("collapsible = true\ncollapse_button = 'middle'\ncollapsed = { style = 'tile', icon = '$cpu', padding = 7 }\n[style.tile]\nbackground = '#123456'\nicon = '$memory'\nicon_size = 12\npadding = 3").unwrap();
    let style = cfg.positions[0].groups[0]
        .collapse
        .as_ref()
        .unwrap()
        .style
        .clone();
    assert_eq!(style.icon, Icon::parse("cpu").map(IconSpec::Native));
    assert_eq!(style.icon_size, 12.0);
    assert_eq!(style.padding, 7.0);
    assert_eq!(style.background, Color::parse("#123456").unwrap());
    assert!(
        parse("collapsible = false\ncollapse_button = 'right'\ncollapsed = { icon = '$cpu' }")
            .unwrap()
            .positions[0]
            .groups[0]
            .collapse
            .is_none()
    );
    // Group reservation must not relax conflicting bindings inside the child.
    let error = Config::parse("[left]\ngroups = ['g']\n[group.g]\nmodules = ['m']\ncollapsible = true\ncollapse_button = 'left'\ncollapsed = { icon = '$cpu' }\n[module.m]\nformat_alt = 'alt'\non_click = { left = ['true'] }").unwrap_err();
    assert!(format!("{error:#}").contains("on_click.left"));
}
