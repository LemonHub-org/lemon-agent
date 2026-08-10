// Executes a plan: runs each step with its tool, then the verification
// command. Entry point convention: `execute_plan(plan) -> String`.
// Failures are raised with `throw`; the engine returns them as errors.

fn execute_plan(plan) {
    log_info("executing plan with " + plan.steps.len + " steps");
    for step in plan.steps {
        log_debug("step: " + step.name + " (tool " + step.tool + ")");
        switch step.tool {
            "read_file" => read_file(step.args.path),
            "write_file" => write_file(step.args.path, step.args.content),
            "append_file" => append_file(step.args.path, step.args.content),
            "list_dir" => list_dir(step.args.path),
            "search_code" => search_code(step.args.query, step.args.path),
            "exec_command" => exec_command(step.args.cmd, step.args.args),
            "git_add_commit" => git_add_commit(step.args.message),
            "llm_query" => llm_query(step.args.prompt),
            "sleep" => sleep(step.args.ms),
            _ => throw "unknown tool in plan step: " + step.tool,
        }
    }
    if "verify" in plan {
        let out = exec_command(plan.verify.cmd, plan.verify.args);
        if out.exit_code != 0 {
            throw "verification failed (exit " + out.exit_code + "): " + out.stderr;
        }
    }
    "plan executed successfully"
}

// Optional self-test entry point: `test_<script_name>()`.
fn test_plan_and_execute() {
    let out = exec_command("git", ["--version"]);
    if out.exit_code == 0 {
        "git is available"
    } else {
        throw "git is unavailable: " + out.stderr;
    }
}
