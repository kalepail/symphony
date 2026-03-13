defmodule SymphonyElixir.RuntimeEnv do
  @moduledoc false

  @overrides_key {__MODULE__, :overrides}

  @spec get(String.t()) :: String.t() | nil
  def get(key) when is_binary(key) do
    System.get_env(key) || Map.get(read_overrides(), key)
  end

  @spec load_dotenv_for_workflow(Path.t()) :: :ok | {:error, String.t()}
  def load_dotenv_for_workflow(workflow_path) when is_binary(workflow_path) do
    with {:ok, merged} <- load_directory(Path.dirname(workflow_path)) do
      :persistent_term.put(@overrides_key, merged)
      :ok
    end
  end

  @spec clear() :: :ok
  def clear do
    :persistent_term.erase(@overrides_key)
    :ok
  end

  defp read_overrides do
    :persistent_term.get(@overrides_key, %{})
  end

  defp load_directory(directory) do
    Enum.reduce_while([".env", ".env.local"], {:ok, %{}}, fn filename, {:ok, acc} ->
      directory
      |> Path.join(filename)
      |> merge_file(acc)
    end)
  end

  defp merge_file(path, acc) do
    if File.regular?(path) do
      case parse_file(path) do
        {:ok, values} -> {:cont, {:ok, Map.merge(acc, values)}}
        {:error, reason} -> {:halt, {:error, reason}}
      end
    else
      {:cont, {:ok, acc}}
    end
  end

  defp parse_file(path) do
    path
    |> File.read()
    |> case do
      {:ok, content} -> parse_content(content, path)
      {:error, reason} -> {:error, "dotenv_error path=#{path} reason=#{inspect(reason)}"}
    end
  end

  defp parse_content(content, path) do
    content
    |> String.split(~r/\R/, trim: false)
    |> Enum.with_index(1)
    |> Enum.reduce_while({:ok, %{}}, fn {line, line_number}, {:ok, acc} ->
      case parse_line(line) do
        :skip -> {:cont, {:ok, acc}}
        {:ok, key, value} -> {:cont, {:ok, Map.put(acc, key, value)}}
        :error -> {:halt, {:error, "dotenv_error path=#{path} line=#{line_number}"}}
      end
    end)
  end

  defp parse_line(line) do
    trimmed = String.trim(line)

    cond do
      trimmed == "" ->
        :skip

      String.starts_with?(trimmed, "#") ->
        :skip

      true ->
        case Regex.run(~r/^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$/, trimmed) do
          [_, key, value] -> {:ok, key, normalize_value(value)}
          _ -> :error
        end
    end
  end

  defp normalize_value(value) do
    trimmed = String.trim(value)

    cond do
      String.starts_with?(trimmed, "\"") and String.ends_with?(trimmed, "\"") ->
        trimmed |> String.trim_leading("\"") |> String.trim_trailing("\"")

      String.starts_with?(trimmed, "'") and String.ends_with?(trimmed, "'") ->
        trimmed |> String.trim_leading("'") |> String.trim_trailing("'")

      true ->
        trimmed
    end
  end
end
