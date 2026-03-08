use super::*;

fn app_is_running(app: &str) -> Result<bool, String> {
    let Some(active_app) = resolve_active_app_ref(app)? else {
        return Ok(false);
    };
    container::is_running(&active_app)
}

fn restart_app_process(app: &str, vars: &BTreeMap<String, String>) -> Result<(), String> {
    let active_app = resolve_existing_active_app_ref(app)?;
    let required_env = read_required_env(&active_app)?;
    ensure_required_env_present(&required_env, vars)?;
    let start = read_start_command(&active_app)?;
    let port = allocate_port(app);
    launch_app_process(&active_app, port, &start, vars)?;
    if tailscale::dns_name_in_container(&active_app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(&active_app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    Ok(())
}

pub fn env_command(app: &str, assignments: &[String]) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let mut vars = read_env_vars(app)?;

    if assignments.is_empty() {
        if vars.is_empty() {
            println!("No environment variables configured for {app}.");
            return Ok(());
        }
        for (name, value) in &vars {
            println!("{name}={value}");
        }
        return Ok(());
    }

    for assignment in assignments {
        let (name, value) = parse_env_assignment(assignment)?;
        vars.insert(name, value);
    }

    let running = app_is_running(app)?;
    if running {
        let active_app = resolve_existing_active_app_ref(app)?;
        let required_env = read_required_env(&active_app)?;
        ensure_required_env_present(&required_env, &vars)?;
    }

    write_env_vars(app, &vars)?;
    eprintln!("-----> Saved {} env var(s) for {app}", assignments.len());

    if running {
        eprintln!("-----> Restarting {app} to apply environment changes");
        restart_app_process(app, &vars)?;
        eprintln!("=====> {app} restarted");
    } else {
        eprintln!("       {app} is not running; changes will apply on next start/deploy");
    }

    Ok(())
}

pub fn env_unset(app: &str, names: &[String]) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    if names.is_empty() {
        return Err("env-unset requires at least one NAME".to_string());
    }

    let mut vars = read_env_vars(app)?;
    let mut parsed_names = Vec::new();
    for name in names {
        let parsed = parse_env_name(name)?;
        if parsed_names.iter().any(|v| v == &parsed) {
            continue;
        }
        parsed_names.push(parsed);
    }

    for name in &parsed_names {
        vars.remove(name);
    }

    let running = app_is_running(app)?;
    if running {
        let active_app = resolve_existing_active_app_ref(app)?;
        let required_env = read_required_env(&active_app)?;
        ensure_required_env_present(&required_env, &vars)?;
    }

    if vars.is_empty() {
        remove_env_vars(app)?;
    } else {
        write_env_vars(app, &vars)?;
    }
    eprintln!("-----> Unset {} env var(s) for {app}", parsed_names.len());

    if running {
        eprintln!("-----> Restarting {app} to apply environment changes");
        restart_app_process(app, &vars)?;
        eprintln!("=====> {app} restarted");
    } else {
        eprintln!("       {app} is not running; changes will apply on next start/deploy");
    }

    Ok(())
}
pub fn stop(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    set_app_desired_state(app, DESIRED_STATE_STOPPED)?;
    eprintln!("-----> Stopping {app}");
    let port = allocate_port(app);
    let _ = stop_app_process_on_port(&active_app, port);
    container::stop(&active_app)?;
    eprintln!("=====> {app} stopped");
    Ok(())
}

pub fn start(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    set_app_desired_state(app, DESIRED_STATE_RUNNING)?;
    eprintln!("-----> Starting {app}");
    if !container::is_running(&active_app)? {
        container::start(&active_app)?;
    }
    if app_service_is_active(&active_app)? {
        eprintln!("       {app} is already running; skipping launch");
        eprintln!("=====> {app} started");
        return Ok(());
    }
    let vars = read_env_vars(app)?;
    let required_env = read_required_env(&active_app)?;
    ensure_required_env_present(&required_env, &vars)?;
    let command = read_start_command(&active_app)?;
    let port = allocate_port(app);
    launch_app_process(&active_app, port, &command, &vars)?;
    if tailscale::dns_name_in_container(&active_app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(&active_app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    eprintln!("=====> {app} started");
    Ok(())
}

pub fn restart(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    set_app_desired_state(app, DESIRED_STATE_RUNNING)?;
    eprintln!("-----> Restarting {app}");
    if container::is_running(&active_app)? {
        container::stop(&active_app)?;
    }
    container::start(&active_app)?;
    let vars = read_env_vars(app)?;
    let required_env = read_required_env(&active_app)?;
    ensure_required_env_present(&required_env, &vars)?;
    let command = read_start_command(&active_app)?;
    let port = allocate_port(app);
    launch_app_process(&active_app, port, &command, &vars)?;
    if tailscale::dns_name_in_container(&active_app).is_some()
        && let Err(e) = tailscale::expose_http_in_container(&active_app, port)
    {
        eprintln!("       Warning: failed to expose tailnet HTTP on :80: {e}");
    }
    eprintln!("=====> {app} restarted");
    Ok(())
}

pub fn destroy(app: &str) -> Result<(), String> {
    app_name::validate_app_name(app)?;
    let active_app = resolve_existing_active_app_ref(app)?;
    let runtime_state = read_app_runtime_state(app)?;
    eprintln!("-----> Destroying {app}");
    if let Err(e) = container::remove_storage_mount(&active_app) {
        eprintln!("       Warning: failed to remove /storage mount before destroy: {e}");
    }
    if let Err(e) = container::remove_tailscale_state_mount(&active_app) {
        eprintln!("       Warning: failed to remove tailscale state mount before destroy: {e}");
    }
    container::stop(&active_app)?;
    container::delete(&active_app)?;

    if let Some(state) = runtime_state
        && let Some(previous_instance) = state.previous_instance
        && let Some(previous_app) = app_ref_from_instance_name(&previous_instance)
        && previous_app != active_app
        && container::exists(&previous_app)
    {
        let _ = container::stop(&previous_app);
        let _ = container::delete(&previous_app);
    }

    delete_app_storage_volume(app)?;
    if let Err(e) = delete_app_tailscale_volume(app) {
        eprintln!("       Warning: failed to remove tailscale state volume: {e}");
    }
    if let Err(e) = cleanup_all_owned_tailscale_devices(app) {
        eprintln!("       Warning: failed to clean up tracked tailscale devices: {e}");
    }
    if let Err(e) = remove_env_vars(app) {
        eprintln!("       Warning: failed to remove env vars: {e}");
    }
    if let Err(e) = clear_app_runtime_state(app) {
        eprintln!("       Warning: failed to clear app runtime state: {e}");
    }
    eprintln!("=====> {app} destroyed");
    Ok(())
}
