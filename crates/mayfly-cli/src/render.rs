//! How `mayfly` prints the daemon's answers: for a person by default, the answer's JSON with
//! `--json` (the documented fields of `mayfly_composition::operator::wire`, pretty-printed;
//! `events --json` prints one compact line per event). An answer this build cannot read for
//! a person (a much newer daemon) is printed as its JSON.

use std::io::Write;

use mayfly_composition::operator::wire::{
    self, EventLine, Following, Lagged, PoolList, PoolStatus, Request, Reset, Status, Value, VmList,
};

fn json(result: Option<&Value>, out: &mut dyn Write) {
    let text = result.map_or_else(String::new, wire::pretty);
    let _ = writeln!(out, "{text}");
}

/// Prints a served request's answer.
pub fn served(request: &Request, result: Option<&Value>, as_json: bool, out: &mut dyn Write) {
    if as_json {
        json(result, out);
        return;
    }
    let printed = match request {
        Request::Status => result
            .and_then(wire::read::<Status>)
            .map(|s| status(&s, out)),
        Request::PoolList => result.and_then(wire::read::<PoolList>).map(|l| {
            for pool in &l.pools {
                pool_lines(pool, out);
            }
        }),
        Request::VmList => result.and_then(wire::read::<VmList>).map(|l| vms(&l, out)),
        Request::PoolResetBreaker { pool, .. } => result.and_then(wire::read::<Reset>).map(|r| {
            let _ = writeln!(out, "pool {pool}: breaker reset for {}", r.reset.join(", "));
        }),
        Request::PoolPause { pool } => done(out, &format!("pool {pool} paused")),
        Request::PoolResume { pool } => done(out, &format!("pool {pool} resumed")),
        Request::PoolDrain { pool } => done(out, &format!("pool {pool} draining")),
        Request::HostDrain { .. } => done(out, "host draining"),
        Request::HostResume => done(out, "host resumed"),
        // `doctor` reads it; no command of the CLI's prints it alone
        Request::Events { .. } | Request::Reaper => None,
    };
    if printed.is_none() {
        json(result, out);
    }
}

fn done(out: &mut dyn Write, line: &str) -> Option<()> {
    let _ = writeln!(out, "{line}");
    Some(())
}

fn status(s: &Status, out: &mut dyn Write) {
    let ready = if s.run.ready { "ready" } else { "not ready" };
    let _ = write!(
        out,
        "host {}  mayflyd {}  run {} epoch {} {} {ready}",
        s.host, s.version, s.run.number, s.run.epoch, s.run.phase
    );
    match &s.run.safe_mode {
        Some(reason) => {
            let _ = writeln!(out, "  SAFE MODE ({reason})");
        }
        None => {
            let _ = writeln!(out);
        }
    }
    for pool in &s.pools {
        pool_lines(pool, out);
    }
    for host in &s.hosts {
        let admission = if host.admission_open {
            "open"
        } else {
            "closed"
        };
        let _ = writeln!(
            out,
            "host {}: admission {admission}, drain {}, leases {}",
            host.host, host.drain, host.leases
        );
    }
    let phases: Vec<String> = s
        .attempts
        .by_phase
        .iter()
        .map(|(phase, n)| format!("{phase}={n}"))
        .collect();
    let _ = writeln!(
        out,
        "attempts: {}; unknown effects {}",
        if phases.is_empty() {
            "none".to_owned()
        } else {
            phases.join(" ")
        },
        s.attempts.unknown_effects
    );
    for desync in &s.attempts.desync {
        let _ = writeln!(
            out,
            "desync: attempt {} (pool {}, vm {}): {}",
            desync.attempt, desync.pool, desync.vm, desync.note
        );
    }
    let sizing: Vec<String> = s
        .sizing
        .iter()
        .map(|v| match &v.reason {
            Some(reason) => format!("{}={} ({}, {reason})", v.key, v.value, v.source),
            None => format!("{}={} ({})", v.key, v.value, v.source),
        })
        .collect();
    let _ = writeln!(out, "sizing: {}", sizing.join(", "));
    for digest in &s.unmeasured_images {
        let _ = writeln!(
            out,
            "sizing: image {digest} not measured (its disk is unknown)"
        );
    }
}

fn pool_lines(pool: &PoolStatus, out: &mut dyn Write) {
    let _ = write!(
        out,
        "pool {}: {}, image {}",
        pool.name,
        pool.mode,
        pool.image.as_deref().unwrap_or("none")
    );
    if pool.tripped.is_empty() {
        let _ = writeln!(out);
    } else {
        let tripped: Vec<&str> = pool.tripped.iter().map(|t| t.digest.as_str()).collect();
        let _ = writeln!(out, ", breaker tripped for {}", tripped.join(", "));
    }
    for on in &pool.hosts {
        let terms = if on.terms_open { "open" } else { "closed" };
        let _ = writeln!(
            out,
            "  on host {}: terms {terms}, placement {}, leases {}",
            on.host,
            on.placement.as_deref().unwrap_or("none"),
            on.leases
        );
    }
}

fn vms(list: &VmList, out: &mut dyn Write) {
    if list.vms.is_empty() {
        let _ = writeln!(out, "no VMs");
    }
    for vm in &list.vms {
        let _ = write!(
            out,
            "{} {} {} {} {}",
            vm.attempt, vm.pool, vm.host, vm.vm, vm.phase
        );
        if let Some(pending) = &vm.pending {
            let unknown = if vm.pending_unknown {
                " (outcome unknown)"
            } else {
                ""
            };
            let _ = write!(out, " waiting on {pending}{unknown}");
        }
        match &vm.note {
            Some(note) => {
                let _ = writeln!(out, ": {note}");
            }
            None => {
                let _ = writeln!(out);
            }
        }
    }
}

/// Prints one streamed `events` answer; `false` once the output cannot be written.
pub fn event(result: &Value, as_json: bool, out: &mut dyn Write) -> bool {
    if wire::read::<Following>(result).is_some() {
        return true;
    }
    let line = if as_json {
        result.to_string()
    } else if let Some(lagged) = wire::read::<Lagged>(result) {
        format!(
            "({} committed units not shown: this follower fell behind)",
            lagged.lagged
        )
    } else if let Some(event) = wire::read::<EventLine>(result) {
        format!(
            "{} {}@{} {} v{} {}",
            event.recorded_at,
            event.stream,
            event.revision,
            event.kind,
            event.schema_version,
            event.payload
        )
    } else {
        result.to_string()
    };
    writeln!(out, "{line}").is_ok() && out.flush().is_ok()
}
