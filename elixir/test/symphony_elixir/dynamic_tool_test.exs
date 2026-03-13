defmodule SymphonyElixir.Codex.DynamicToolTest do
  use SymphonyElixir.TestSupport

  alias SymphonyElixir.Codex.DynamicTool
  alias SymphonyElixir.RuntimeEnv

  test "tool_specs advertises the linear_graphql input contract" do
    specs = DynamicTool.tool_specs(github_api_available?: false)

    assert [
             %{
               "description" => description,
               "inputSchema" => %{
                 "properties" => %{
                   "query" => _,
                   "variables" => _
                 },
                 "required" => ["query"],
                 "type" => "object"
               },
               "name" => "linear_graphql"
             }
           ] = specs

    assert description =~ "Linear"
  end

  test "tool_specs advertises github_api when host-side GitHub auth is available" do
    assert [
             %{"name" => "linear_graphql"},
             %{
               "description" => description,
               "inputSchema" => %{
                 "properties" => %{
                   "body" => _,
                   "method" => _,
                   "path" => _
                 },
                 "required" => ["method", "path"],
                 "type" => "object"
               },
               "name" => "github_api"
             }
           ] = DynamicTool.tool_specs(github_api_available?: true)

    assert description =~ "GitHub REST API"
  end

  test "unsupported tools return a failure payload with the supported tool list" do
    response = DynamicTool.execute("not_a_real_tool", %{}, github_api_available?: false)

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "error" => %{
               "message" => ~s(Unsupported dynamic tool: "not_a_real_tool".),
               "supportedTools" => ["linear_graphql"]
             }
           }

    assert response["contentItems"] == [
             %{
               "type" => "inputText",
               "text" => response["output"]
             }
           ]
  end

  test "linear_graphql returns successful GraphQL responses as tool text" do
    test_pid = self()

    response =
      DynamicTool.execute(
        "linear_graphql",
        %{
          "query" => "query Viewer { viewer { id } }",
          "variables" => %{"includeTeams" => false}
        },
        linear_client: fn query, variables, opts ->
          send(test_pid, {:linear_client_called, query, variables, opts})
          {:ok, %{"data" => %{"viewer" => %{"id" => "usr_123"}}}}
        end
      )

    assert_received {:linear_client_called, "query Viewer { viewer { id } }", %{"includeTeams" => false}, []}

    assert response["success"] == true
    assert Jason.decode!(response["output"]) == %{"data" => %{"viewer" => %{"id" => "usr_123"}}}
    assert response["contentItems"] == [%{"type" => "inputText", "text" => response["output"]}]
  end

  test "linear_graphql accepts a raw GraphQL query string" do
    test_pid = self()

    response =
      DynamicTool.execute(
        "linear_graphql",
        "  query Viewer { viewer { id } }  ",
        linear_client: fn query, variables, opts ->
          send(test_pid, {:linear_client_called, query, variables, opts})
          {:ok, %{"data" => %{"viewer" => %{"id" => "usr_456"}}}}
        end
      )

    assert_received {:linear_client_called, "query Viewer { viewer { id } }", %{}, []}
    assert response["success"] == true
  end

  test "linear_graphql ignores legacy operationName arguments" do
    test_pid = self()

    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }", "operationName" => "Viewer"},
        linear_client: fn query, variables, opts ->
          send(test_pid, {:linear_client_called, query, variables, opts})
          {:ok, %{"data" => %{"viewer" => %{"id" => "usr_789"}}}}
        end
      )

    assert_received {:linear_client_called, "query Viewer { viewer { id } }", %{}, []}
    assert response["success"] == true
  end

  test "linear_graphql passes multi-operation documents through unchanged" do
    test_pid = self()

    query = """
    query Viewer { viewer { id } }
    query Teams { teams { nodes { id } } }
    """

    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => query},
        linear_client: fn forwarded_query, variables, opts ->
          send(test_pid, {:linear_client_called, forwarded_query, variables, opts})
          {:ok, %{"errors" => [%{"message" => "Must provide operation name if query contains multiple operations."}]}}
        end
      )

    assert_received {:linear_client_called, forwarded_query, %{}, []}
    assert forwarded_query == String.trim(query)
    assert response["success"] == false
  end

  test "linear_graphql rejects blank raw query strings even when using the default client" do
    response = DynamicTool.execute("linear_graphql", "   ")

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "error" => %{
               "message" => "`linear_graphql` requires a non-empty `query` string."
             }
           }
  end

  test "linear_graphql marks GraphQL error responses as failures while preserving the body" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "mutation BadMutation { nope }"},
        linear_client: fn _query, _variables, _opts ->
          {:ok, %{"errors" => [%{"message" => "Unknown field `nope`"}], "data" => nil}}
        end
      )

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "data" => nil,
             "errors" => [%{"message" => "Unknown field `nope`"}]
           }
  end

  test "linear_graphql marks atom-key GraphQL error responses as failures" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }"},
        linear_client: fn _query, _variables, _opts ->
          {:ok, %{errors: [%{message: "boom"}], data: nil}}
        end
      )

    assert response["success"] == false
  end

  test "linear_graphql validates required arguments before calling Linear" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"variables" => %{"commentId" => "comment-1"}},
        linear_client: fn _query, _variables, _opts ->
          flunk("linear client should not be called when arguments are invalid")
        end
      )

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "error" => %{
               "message" => "`linear_graphql` requires a non-empty `query` string."
             }
           }

    blank_query =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "   "},
        linear_client: fn _query, _variables, _opts ->
          flunk("linear client should not be called when the query is blank")
        end
      )

    assert blank_query["success"] == false
  end

  test "linear_graphql rejects invalid argument types" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        [:not, :valid],
        linear_client: fn _query, _variables, _opts ->
          flunk("linear client should not be called when arguments are invalid")
        end
      )

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "error" => %{
               "message" => "`linear_graphql` expects either a GraphQL query string or an object with `query` and optional `variables`."
             }
           }
  end

  test "linear_graphql rejects invalid variables" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }", "variables" => ["bad"]},
        linear_client: fn _query, _variables, _opts ->
          flunk("linear client should not be called when variables are invalid")
        end
      )

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "error" => %{
               "message" => "`linear_graphql.variables` must be a JSON object when provided."
             }
           }
  end

  test "linear_graphql formats transport and auth failures" do
    missing_token =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }"},
        linear_client: fn _query, _variables, _opts -> {:error, :missing_linear_api_token} end
      )

    assert missing_token["success"] == false

    assert Jason.decode!(missing_token["output"]) == %{
             "error" => %{
               "message" => "Symphony is missing Linear auth. Set `linear.api_key` in `WORKFLOW.md` or export `LINEAR_API_KEY`."
             }
           }

    status_error =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }"},
        linear_client: fn _query, _variables, _opts -> {:error, {:linear_api_status, 503}} end
      )

    assert Jason.decode!(status_error["output"]) == %{
             "error" => %{
               "message" => "Linear GraphQL request failed with HTTP 503.",
               "status" => 503
             }
           }

    request_error =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }"},
        linear_client: fn _query, _variables, _opts -> {:error, {:linear_api_request, :timeout}} end
      )

    assert Jason.decode!(request_error["output"]) == %{
             "error" => %{
               "message" => "Linear GraphQL request failed before receiving a successful response.",
               "reason" => ":timeout"
             }
           }
  end

  test "linear_graphql formats unexpected failures from the client" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }"},
        linear_client: fn _query, _variables, _opts -> {:error, :boom} end
      )

    assert response["success"] == false

    assert Jason.decode!(response["output"]) == %{
             "error" => %{
               "message" => "Linear GraphQL tool execution failed.",
               "reason" => ":boom"
             }
           }
  end

  test "linear_graphql falls back to inspect for non-JSON payloads" do
    response =
      DynamicTool.execute(
        "linear_graphql",
        %{"query" => "query Viewer { viewer { id } }"},
        linear_client: fn _query, _variables, _opts -> {:ok, :ok} end
      )

    assert response["success"] == true
    assert response["output"] == ":ok"
  end

  test "github_api normalizes arguments and returns successful responses as tool text" do
    test_pid = self()

    response =
      DynamicTool.execute(
        "github_api",
        %{
          "method" => "post",
          "path" => "repos/owner/repo/issues/14/labels",
          "body" => ~s({"labels":["symphony"]})
        },
        github_token_provider: fn -> {:ok, "github-token"} end,
        github_requester: fn method, path, body, token, base_url ->
          send(test_pid, {:github_request, method, path, body, token, base_url})
          {:ok, %{"ok" => true, "labels" => ["symphony"]}}
        end
      )

    assert_received {:github_request, "POST", "/repos/owner/repo/issues/14/labels", %{"labels" => ["symphony"]}, "github-token", "https://api.github.com"}

    assert response["success"] == true

    assert [
             %{
               "text" => text
             }
           ] = response["contentItems"]

    assert Jason.decode!(text) == %{"ok" => true, "labels" => ["symphony"]}
  end

  test "github_api reads token and base url from workflow-local dotenv files" do
    test_pid = self()
    workflow_file = Workflow.workflow_file_path()
    workflow_dir = Path.dirname(workflow_file)
    dotenv_path = Path.join(workflow_dir, ".env.local")

    File.write!(
      dotenv_path,
      """
      GH_TOKEN=dotenv-github-token
      SYMPHONY_GITHUB_API_URL=https://example.test/
      """
    )

    original_gh_token = System.get_env("GH_TOKEN")
    original_github_token = System.get_env("GITHUB_TOKEN")
    original_api_url = System.get_env("SYMPHONY_GITHUB_API_URL")

    System.delete_env("GH_TOKEN")
    System.delete_env("GITHUB_TOKEN")
    System.delete_env("SYMPHONY_GITHUB_API_URL")
    assert :ok = RuntimeEnv.load_dotenv_for_workflow(workflow_file)

    on_exit(fn ->
      RuntimeEnv.clear()
      restore_env("GH_TOKEN", original_gh_token)
      restore_env("GITHUB_TOKEN", original_github_token)
      restore_env("SYMPHONY_GITHUB_API_URL", original_api_url)
    end)

    response =
      DynamicTool.execute(
        "github_api",
        %{"method" => "GET", "path" => "/repos/owner/repo/pulls/14"},
        github_requester: fn method, path, body, token, base_url ->
          send(test_pid, {:github_request, method, path, body, token, base_url})
          {:ok, %{"ok" => true}}
        end
      )

    assert_received {:github_request, "GET", "/repos/owner/repo/pulls/14", nil, "dotenv-github-token", "https://example.test"}
    assert response["success"] == true
  end

  test "github_api validates required arguments before calling GitHub" do
    response =
      DynamicTool.execute(
        "github_api",
        %{"path" => "/repos/owner/repo/pulls"},
        github_token_provider: fn ->
          flunk("github token provider should not be called when arguments are invalid")
        end
      )

    assert response["success"] == false

    assert [
             %{
               "text" => text
             }
           ] = response["contentItems"]

    assert Jason.decode!(text) == %{
             "error" => %{
               "message" => "`github_api.method` is required."
             }
           }

    invalid_method =
      DynamicTool.execute(
        "github_api",
        %{"method" => "NOT_REAL", "path" => "/repos/owner/repo/pulls"},
        github_token_provider: fn ->
          flunk("github token provider should not be called when the method is invalid")
        end
      )

    assert [
             %{
               "text" => invalid_method_text
             }
           ] = invalid_method["contentItems"]

    assert Jason.decode!(invalid_method_text) == %{
             "error" => %{
               "message" => "`github_api.method` must be a valid HTTP method."
             }
           }
  end

  test "github_api formats auth and HTTP failures" do
    auth_error =
      DynamicTool.execute(
        "github_api",
        %{"method" => "GET", "path" => "/repos/owner/repo/pulls/14"},
        github_token_provider: fn -> {:error, :github_auth_unavailable} end
      )

    assert auth_error["success"] == false

    assert [
             %{
               "text" => auth_error_text
             }
           ] = auth_error["contentItems"]

    assert Jason.decode!(auth_error_text) == %{
             "error" => %{
               "message" => "GitHub API tool is unavailable because no auth token could be found."
             }
           }

    status_error =
      DynamicTool.execute(
        "github_api",
        %{"method" => "GET", "path" => "/repos/owner/repo/pulls/14"},
        github_token_provider: fn -> {:ok, "github-token"} end,
        github_requester: fn _method, _path, _body, _token, _base_url ->
          {:error, {:github_api_status, 422, "GET", "/repos/owner/repo/pulls/14", %{"message" => "unprocessable"}}}
        end
      )

    assert [
             %{
               "text" => status_error_text
             }
           ] = status_error["contentItems"]

    assert Jason.decode!(status_error_text) == %{
             "error" => %{
               "message" => "GitHub API request failed with HTTP 422.",
               "method" => "GET",
               "path" => "/repos/owner/repo/pulls/14",
               "response" => %{"message" => "unprocessable"},
               "status" => 422
             }
           }
  end
end
