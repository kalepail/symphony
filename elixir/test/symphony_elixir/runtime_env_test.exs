defmodule SymphonyElixir.RuntimeEnvTest do
  use ExUnit.Case, async: false

  alias SymphonyElixir.{Config.Schema, RuntimeEnv, Workflow}

  setup do
    previous_linear_api_key = System.get_env("LINEAR_API_KEY")
    previous_workspace_root = System.get_env("SYMPHONY_WORKSPACE_ROOT")

    on_exit(fn ->
      restore_env("LINEAR_API_KEY", previous_linear_api_key)
      restore_env("SYMPHONY_WORKSPACE_ROOT", previous_workspace_root)
      RuntimeEnv.clear()
    end)

    RuntimeEnv.clear()
    :ok
  end

  test "load_dotenv_for_workflow prefers workflow directory and env.local overrides" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")
    File.write!(Path.join(root, ".env"), "LINEAR_API_KEY=root-token\n")
    File.write!(Path.join(workflow_dir, ".env"), "LINEAR_API_KEY=workflow-token\n")
    File.write!(Path.join(workflow_dir, ".env.local"), "LINEAR_API_KEY=local-token\n")

    assert :ok = RuntimeEnv.load_dotenv_for_workflow(workflow_path)
    assert RuntimeEnv.get("LINEAR_API_KEY") == "local-token"
    assert System.get_env("LINEAR_API_KEY") == nil
  end

  test "dotenv overrides are visible to other processes" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")
    File.write!(Path.join(workflow_dir, ".env.local"), "LINEAR_API_KEY=shared-token\n")

    assert :ok = RuntimeEnv.load_dotenv_for_workflow(workflow_path)

    parent = self()

    pid =
      spawn(fn ->
        send(parent, {:runtime_env_value, RuntimeEnv.get("LINEAR_API_KEY")})
      end)

    assert is_pid(pid)
    assert_receive {:runtime_env_value, "shared-token"}
  end

  test "workflow load resolves tracker config through dotenv overlay" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")

    File.write!(
      workflow_path,
      """
      ---
      tracker:
        kind: linear
        api_key: $LINEAR_API_KEY
        project_slug: smoke-proj
      workspace:
        root: $SYMPHONY_WORKSPACE_ROOT
      ---

      Prompt
      """
    )

    File.write!(
      Path.join(workflow_dir, ".env"),
      "LINEAR_API_KEY=dotenv-token\nSYMPHONY_WORKSPACE_ROOT=/tmp/elixir-dotenv-root\n"
    )

    assert {:ok, loaded} = Workflow.load(workflow_path)
    assert {:ok, settings} = Schema.parse(loaded.config)
    assert settings.tracker.api_key == "dotenv-token"
    assert settings.workspace.root == "/tmp/elixir-dotenv-root"
    assert System.get_env("LINEAR_API_KEY") == nil
    assert System.get_env("SYMPHONY_WORKSPACE_ROOT") == nil
  end

  test "system environment wins over dotenv overlay" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")
    File.write!(Path.join(workflow_dir, ".env"), "LINEAR_API_KEY=dotenv-token\n")
    System.put_env("LINEAR_API_KEY", "shell-token")

    assert :ok = RuntimeEnv.load_dotenv_for_workflow(workflow_path)
    assert RuntimeEnv.get("LINEAR_API_KEY") == "shell-token"
  end

  test "load_dotenv_for_workflow parses export prefixes, quoted values, comments, and blank lines" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")

    File.write!(
      Path.join(workflow_dir, ".env"),
      """
      # comment

      export LINEAR_API_KEY="quoted-token"
      SYMPHONY_WORKSPACE_ROOT='/tmp/quoted-root'
      """
    )

    assert :ok = RuntimeEnv.load_dotenv_for_workflow(workflow_path)
    assert RuntimeEnv.get("LINEAR_API_KEY") == "quoted-token"
    assert RuntimeEnv.get("SYMPHONY_WORKSPACE_ROOT") == "/tmp/quoted-root"
  end

  test "load_dotenv_for_workflow returns an error for invalid dotenv syntax" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")
    File.write!(Path.join(workflow_dir, ".env"), "not valid\n")

    assert {:error, reason} = RuntimeEnv.load_dotenv_for_workflow(workflow_path)
    assert reason =~ "dotenv_error"
    assert reason =~ "line=1"
  end

  test "load_dotenv_for_workflow succeeds when no dotenv files exist and clear removes overrides" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")

    assert :ok = RuntimeEnv.load_dotenv_for_workflow(workflow_path)
    assert RuntimeEnv.get("LINEAR_API_KEY") == nil

    :persistent_term.put({RuntimeEnv, :overrides}, %{"LINEAR_API_KEY" => "overlay-token"})
    assert :ok = RuntimeEnv.clear()
    assert RuntimeEnv.get("LINEAR_API_KEY") == nil
  end

  test "load_dotenv_for_workflow returns an error for unreadable dotenv files" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    dotenv_path = Path.join(workflow_dir, ".env")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")
    File.write!(dotenv_path, "LINEAR_API_KEY=secret\n")
    File.chmod!(dotenv_path, 0o000)

    on_exit(fn -> File.chmod(dotenv_path, 0o644) end)

    assert {:error, reason} = RuntimeEnv.load_dotenv_for_workflow(workflow_path)
    assert reason =~ "dotenv_error"
    assert reason =~ Path.basename(dotenv_path)
  end

  test "workflow load surfaces dotenv parsing errors" do
    root = Path.join(System.tmp_dir!(), "runtime-env-test-#{System.unique_integer([:positive])}")
    workflow_dir = Path.join(root, "workflow")
    File.mkdir_p!(workflow_dir)
    workflow_path = Path.join(workflow_dir, "WORKFLOW.md")
    File.write!(workflow_path, "---\ntracker:\n  kind: linear\n---\n")
    File.write!(Path.join(workflow_dir, ".env"), "still not valid\n")

    assert {:error, {:workflow_env_error, reason}} = Workflow.load(workflow_path)
    assert reason =~ "dotenv_error"
  end

  defp restore_env(key, nil), do: System.delete_env(key)
  defp restore_env(key, value), do: System.put_env(key, value)
end
