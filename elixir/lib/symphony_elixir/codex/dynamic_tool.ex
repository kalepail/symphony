defmodule SymphonyElixir.Codex.DynamicTool do
  @moduledoc """
  Executes client-side tool calls requested by Codex app-server turns.
  """

  alias SymphonyElixir.Linear.Client

  @github_api_tool "github_api"
  @github_api_base_url_env "SYMPHONY_GITHUB_API_URL"
  @github_api_default_base_url "https://api.github.com"
  @github_api_description """
  Execute a GitHub REST API request using host-side auth from Symphony's runtime.
  Use this when `gh` transport is flaky inside Codex sessions but the host can still reach GitHub.
  Create pull requests with POST `/repos/{owner}/{repo}/pulls` and JSON body fields `title`, `head`, `base`, `body`.
  Add labels with POST `/repos/{owner}/{repo}/issues/{number}/labels` and body `{ "labels": ["symphony"] }`.
  Read pull requests with GET `/repos/{owner}/{repo}/pulls/{number}`.
  Read CI checks with GET `/repos/{owner}/{repo}/commits/{sha}/check-runs`.
  """
  @github_api_input_schema %{
    "type" => "object",
    "additionalProperties" => false,
    "required" => ["method", "path"],
    "properties" => %{
      "method" => %{
        "type" => "string",
        "description" => "HTTP method to send to the GitHub REST API (GET, POST, PATCH, PUT, DELETE)."
      },
      "path" => %{
        "type" => "string",
        "description" => "API path beginning with `/`, for example `/repos/owner/repo/pulls`."
      },
      "body" => %{
        "description" => "Optional JSON body for POST/PATCH/PUT requests."
      }
    }
  }

  @linear_graphql_tool "linear_graphql"
  @linear_graphql_description """
  Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.
  """
  @linear_graphql_input_schema %{
    "type" => "object",
    "additionalProperties" => false,
    "required" => ["query"],
    "properties" => %{
      "query" => %{
        "type" => "string",
        "description" => "GraphQL query or mutation document to execute against Linear."
      },
      "variables" => %{
        "type" => ["object", "null"],
        "description" => "Optional GraphQL variables object.",
        "additionalProperties" => true
      }
    }
  }

  @spec execute(String.t() | nil, term(), keyword()) :: map()
  def execute(tool, arguments, opts \\ []) do
    case tool do
      @github_api_tool ->
        execute_github_api(arguments, opts)

      @linear_graphql_tool ->
        execute_linear_graphql(arguments, opts)

      other ->
        failure_response(%{
          "error" => %{
            "message" => "Unsupported dynamic tool: #{inspect(other)}.",
            "supportedTools" => supported_tool_names(opts)
          }
        })
    end
  end

  @spec tool_specs(keyword()) :: [map()]
  def tool_specs(opts \\ []) do
    linear_spec = %{
      "name" => @linear_graphql_tool,
      "description" => @linear_graphql_description,
      "inputSchema" => @linear_graphql_input_schema
    }

    github_spec = %{
      "name" => @github_api_tool,
      "description" => @github_api_description,
      "inputSchema" => @github_api_input_schema
    }

    if github_api_available?(opts) do
      [linear_spec, github_spec]
    else
      [linear_spec]
    end
  end

  defp execute_linear_graphql(arguments, opts) do
    linear_client = Keyword.get(opts, :linear_client, &Client.graphql/3)

    with {:ok, query, variables} <- normalize_linear_graphql_arguments(arguments),
         {:ok, response} <- linear_client.(query, variables, []) do
      graphql_response(response)
    else
      {:error, reason} ->
        failure_response(tool_error_payload(reason))
    end
  end

  defp normalize_linear_graphql_arguments(arguments) when is_binary(arguments) do
    case String.trim(arguments) do
      "" -> {:error, :missing_query}
      query -> {:ok, query, %{}}
    end
  end

  defp normalize_linear_graphql_arguments(arguments) when is_map(arguments) do
    case normalize_query(arguments) do
      {:ok, query} ->
        case normalize_variables(arguments) do
          {:ok, variables} ->
            {:ok, query, variables}

          {:error, reason} ->
            {:error, reason}
        end

      {:error, reason} ->
        {:error, reason}
    end
  end

  defp normalize_linear_graphql_arguments(_arguments), do: {:error, :invalid_arguments}

  defp normalize_query(arguments) do
    case Map.get(arguments, "query") || Map.get(arguments, :query) do
      query when is_binary(query) ->
        case String.trim(query) do
          "" -> {:error, :missing_query}
          trimmed -> {:ok, trimmed}
        end

      _ ->
        {:error, :missing_query}
    end
  end

  defp normalize_variables(arguments) do
    case Map.get(arguments, "variables") || Map.get(arguments, :variables) || %{} do
      variables when is_map(variables) -> {:ok, variables}
      _ -> {:error, :invalid_variables}
    end
  end

  defp graphql_response(response) do
    success =
      case response do
        %{"errors" => errors} when is_list(errors) and errors != [] -> false
        %{errors: errors} when is_list(errors) and errors != [] -> false
        _ -> true
      end

    %{
      "success" => success,
      "contentItems" => [
        %{
          "type" => "inputText",
          "text" => encode_payload(response)
        }
      ]
    }
  end

  defp failure_response(payload) do
    %{
      "success" => false,
      "contentItems" => [
        %{
          "type" => "inputText",
          "text" => encode_payload(payload)
        }
      ]
    }
  end

  defp encode_payload(payload) when is_map(payload) or is_list(payload) do
    Jason.encode!(payload, pretty: true)
  end

  defp encode_payload(payload), do: inspect(payload)

  defp tool_error_payload(:missing_query) do
    %{
      "error" => %{
        "message" => "`linear_graphql` requires a non-empty `query` string."
      }
    }
  end

  defp tool_error_payload(:invalid_arguments) do
    %{
      "error" => %{
        "message" => "`linear_graphql` expects either a GraphQL query string or an object with `query` and optional `variables`."
      }
    }
  end

  defp tool_error_payload(:invalid_variables) do
    %{
      "error" => %{
        "message" => "`linear_graphql.variables` must be a JSON object when provided."
      }
    }
  end

  defp tool_error_payload(:missing_linear_api_token) do
    %{
      "error" => %{
        "message" => "Symphony is missing Linear auth. Set `linear.api_key` in `WORKFLOW.md` or export `LINEAR_API_KEY`."
      }
    }
  end

  defp tool_error_payload({:linear_api_status, status}) do
    %{
      "error" => %{
        "message" => "Linear GraphQL request failed with HTTP #{status}.",
        "status" => status
      }
    }
  end

  defp tool_error_payload({:linear_api_request, reason}) do
    %{
      "error" => %{
        "message" => "Linear GraphQL request failed before receiving a successful response.",
        "reason" => inspect(reason)
      }
    }
  end

  defp tool_error_payload(reason) do
    %{
      "error" => %{
        "message" => "Linear GraphQL tool execution failed.",
        "reason" => inspect(reason)
      }
    }
  end

  defp execute_github_api(arguments, opts) do
    requester = Keyword.get(opts, :github_requester, &default_github_request/5)

    with {:ok, method, path, body} <- normalize_github_arguments(arguments),
         {:ok, token} <- github_auth_token(opts),
         {:ok, response} <- requester.(method, path, body, token, github_api_base_url(opts)) do
      success_response(response)
    else
      {:error, {:invalid_github_arguments, payload}} ->
        failure_response(payload)

      {:error, reason} ->
        failure_response(github_tool_error_payload(reason))
    end
  end

  defp normalize_github_arguments(arguments) when is_map(arguments) do
    with {:ok, method} <- normalize_github_method(arguments),
         {:ok, path} <- normalize_github_path(arguments) do
      {:ok, method, path, normalize_github_body(Map.get(arguments, "body") || Map.get(arguments, :body))}
    end
  end

  defp normalize_github_arguments(_arguments) do
    {:error,
     {:invalid_github_arguments,
      %{
        "error" => %{
          "message" => "`github_api` expects an object with `method`, `path`, and optional `body`."
        }
      }}}
  end

  defp normalize_github_method(arguments) do
    case Map.get(arguments, "method") || Map.get(arguments, :method) do
      method when is_binary(method) ->
        method = method |> String.trim() |> String.upcase()

        cond do
          method == "" ->
            {:error, {:invalid_github_arguments, %{"error" => %{"message" => "`github_api.method` is required."}}}}

          method in ~w(GET POST PATCH PUT DELETE HEAD OPTIONS) ->
            {:ok, method}

          true ->
            {:error, {:invalid_github_arguments, %{"error" => %{"message" => "`github_api.method` must be a valid HTTP method."}}}}
        end

      _ ->
        {:error, {:invalid_github_arguments, %{"error" => %{"message" => "`github_api.method` is required."}}}}
    end
  end

  defp normalize_github_path(arguments) do
    case Map.get(arguments, "path") || Map.get(arguments, :path) do
      path when is_binary(path) ->
        path = String.trim(path)

        cond do
          path == "" ->
            {:error, {:invalid_github_arguments, %{"error" => %{"message" => "`github_api.path` is required."}}}}

          String.starts_with?(path, "/") ->
            {:ok, path}

          true ->
            {:ok, "/" <> path}
        end

      _ ->
        {:error, {:invalid_github_arguments, %{"error" => %{"message" => "`github_api.path` is required."}}}}
    end
  end

  defp normalize_github_body(nil), do: nil
  defp normalize_github_body(""), do: nil

  defp normalize_github_body(raw) when is_binary(raw) do
    trimmed = String.trim(raw)

    if trimmed == "" do
      nil
    else
      case Jason.decode(trimmed) do
        {:ok, parsed} -> parsed
        {:error, _reason} -> raw
      end
    end
  end

  defp normalize_github_body(body), do: body

  defp github_api_available?(opts) do
    case Keyword.fetch(opts, :github_api_available?) do
      {:ok, value} ->
        value

      :error ->
        match?({:ok, _}, github_auth_token(opts))
    end
  end

  defp github_api_base_url(opts) do
    Keyword.get_lazy(opts, :github_api_base_url, fn ->
      @github_api_base_url_env
      |> System.get_env()
      |> case do
        nil -> @github_api_default_base_url
        value -> value |> String.trim() |> String.trim_trailing("/")
      end
    end)
  end

  defp github_auth_token(opts) do
    case Keyword.fetch(opts, :github_token_provider) do
      {:ok, provider} ->
        provider.()

      :error ->
        github_token_from_host()
    end
  end

  defp github_token_from_host do
    case github_token_from_env() do
      {:ok, token} ->
        {:ok, token}

      :error ->
        github_token_from_gh()
    end
  end

  defp github_token_from_env do
    ["GH_TOKEN", "GITHUB_TOKEN"]
    |> Enum.find_value(:error, fn key ->
      normalize_github_env_token(System.get_env(key))
    end)
  end

  defp github_token_from_gh do
    System.find_executable("gh")
    |> github_token_from_gh_executable()
  rescue
    error ->
      {:error, {:github_auth_token_failed, Exception.message(error)}}
  end

  defp normalize_github_env_token(value) when is_binary(value) do
    case String.trim(value) do
      "" -> false
      token -> {:ok, token}
    end
  end

  defp normalize_github_env_token(_value), do: false

  defp github_token_from_gh_executable(nil), do: {:error, :github_auth_unavailable}

  defp github_token_from_gh_executable(_path) do
    case System.cmd("gh", ["auth", "token"], stderr_to_stdout: true) do
      {token, 0} ->
        normalize_github_cli_token(token)

      {reason, _status} ->
        {:error, {:github_auth_token_failed, String.trim(reason)}}
    end
  end

  defp normalize_github_cli_token(token) do
    case String.trim(token) do
      "" -> {:error, :github_empty_auth_token}
      value -> {:ok, value}
    end
  end

  defp default_github_request(method, path, body, token, base_url) do
    headers = [
      {"authorization", "Bearer #{token}"},
      {"accept", "application/vnd.github+json"},
      {"x-github-api-version", "2022-11-28"},
      {"user-agent", "symphony-elixir/github_api"}
    ]

    request =
      [
        method: method,
        url: base_url <> path,
        headers: headers
      ]
      |> maybe_put_github_body(body)

    case Req.request(request) do
      {:ok, %Req.Response{status: status, body: response_body}} when status in 200..299 ->
        {:ok, response_body}

      {:ok, %Req.Response{status: status, body: response_body}} ->
        {:error, {:github_api_status, status, method, path, response_body}}

      {:error, reason} ->
        {:error, {:github_api_request, method, path, format_error_reason(reason)}}
    end
  end

  defp maybe_put_github_body(request, nil), do: request
  defp maybe_put_github_body(request, body), do: Keyword.put(request, :json, body)

  defp format_error_reason(reason), do: Exception.message(reason)

  defp github_tool_error_payload(:github_auth_unavailable) do
    %{
      "error" => %{
        "message" => "GitHub API tool is unavailable because no auth token could be found."
      }
    }
  end

  defp github_tool_error_payload(:github_empty_auth_token) do
    %{
      "error" => %{
        "message" => "GitHub API tool received an empty auth token."
      }
    }
  end

  defp github_tool_error_payload({:github_auth_token_failed, reason}) do
    %{
      "error" => %{
        "message" => "GitHub API tool could not retrieve a host auth token from `gh auth token`.",
        "reason" => reason
      }
    }
  end

  defp github_tool_error_payload({:github_api_request, method, path, reason}) do
    %{
      "error" => %{
        "message" => "GitHub API request failed before receiving a response.",
        "reason" => reason,
        "method" => method,
        "path" => path
      }
    }
  end

  defp github_tool_error_payload({:github_api_status, status, method, path, response_body}) do
    %{
      "error" => %{
        "message" => "GitHub API request failed with HTTP #{status}.",
        "status" => status,
        "method" => method,
        "path" => path,
        "response" => parse_payload(response_body)
      }
    }
  end

  defp github_tool_error_payload(reason) do
    %{
      "error" => %{
        "message" => "GitHub API tool execution failed.",
        "reason" => inspect(reason)
      }
    }
  end

  defp success_response(response) do
    %{
      "success" => true,
      "contentItems" => [
        %{
          "type" => "inputText",
          "text" => encode_payload(response)
        }
      ]
    }
  end

  defp parse_payload(payload) when is_binary(payload) do
    case Jason.decode(payload) do
      {:ok, decoded} -> decoded
      {:error, _reason} -> payload
    end
  end

  defp parse_payload(payload), do: payload

  defp supported_tool_names(opts) do
    Enum.map(tool_specs(opts), & &1["name"])
  end
end
