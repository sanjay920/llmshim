# frozen_string_literal: true

require_relative "test_helper"

class LlmshimClientTest < Minitest::Test
  def teardown
    @proxy&.shutdown
  end

  def client_for(routes)
    @proxy = MockProxy.new(routes)
    Llmshim::Client.new(base_url: @proxy.base_url, timeout: 5)
  end

  # --- POST /v1/chat -------------------------------------------------------

  def test_chat_parses_chat_response
    body = {
      id: "resp_123",
      model: "claude-sonnet-4-6",
      provider: "anthropic",
      message: {
        role: "assistant",
        content: "Hello there!",
        tool_calls: [
          { id: "call_1", type: "function",
            function: { name: "get_weather", arguments: '{"city":"SF"}' } }
        ]
      },
      reasoning: "thinking...",
      usage: { input_tokens: 10, output_tokens: 5, reasoning_tokens: 3, total_tokens: 18 },
      latency_ms: 1234
    }

    client = client_for(
      "/v1/chat" => lambda do |_req, res|
        res.content_type = "application/json"
        res.body = JSON.generate(body)
      end
    )

    resp = client.chat(model: "claude-sonnet-4-6", messages: "Hi")

    assert_instance_of Llmshim::ChatResponse, resp
    assert_equal "resp_123", resp.id
    assert_equal "anthropic", resp.provider
    assert_equal "Hello there!", resp.content
    assert_equal "thinking...", resp.reasoning
    assert_equal 1234, resp.latency_ms
    assert_equal 18, resp.usage.total_tokens
    assert_equal 3, resp.usage.reasoning_tokens

    tc = resp.message.tool_calls.first
    assert_equal "call_1", tc.id
    assert_equal "get_weather", tc.name
    assert_equal '{"city":"SF"}', tc.arguments
  end

  def test_chat_serializes_config_and_provider_config
    captured = {}
    client = client_for(
      "/v1/chat" => lambda do |req, res|
        captured.merge!(JSON.parse(req.body))
        res.content_type = "application/json"
        res.body = JSON.generate(
          id: "x", model: "gpt-5.5", provider: "openai",
          message: { role: "assistant", content: "ok" },
          usage: {}, latency_ms: 1
        )
      end
    )

    client.chat(
      model: "gpt-5.5",
      messages: [{ "role" => "user", "content" => "Hi" }],
      max_tokens: 100,
      temperature: 0.7,
      reasoning_effort: "high",
      tools: [{ type: "function", function: { name: "f" } }],
      tool_choice: "auto",
      provider_config: { "x-anthropic" => { thinking: { type: "enabled", budget_tokens: 4000 } } },
      fallback: ["gemini/gemini-3.5-flash"]
    )

    assert_equal "gpt-5.5", captured["model"]
    assert_equal [{ "role" => "user", "content" => "Hi" }], captured["messages"]
    assert_equal 100, captured.dig("config", "max_tokens")
    assert_in_delta 0.7, captured.dig("config", "temperature")
    assert_equal "high", captured.dig("config", "reasoning_effort")
    assert_equal "auto", captured.dig("provider_config", "tool_choice")
    assert_equal "enabled", captured.dig("provider_config", "x-anthropic", "thinking", "type")
    assert_equal 4000, captured.dig("provider_config", "x-anthropic", "thinking", "budget_tokens")
    assert_equal ["gemini/gemini-3.5-flash"], captured["fallback"]
  end

  def test_chat_string_message_becomes_user_role
    captured = {}
    client = client_for(
      "/v1/chat" => lambda do |req, res|
        captured.merge!(JSON.parse(req.body))
        res.content_type = "application/json"
        res.body = JSON.generate(
          id: "x", model: "m", provider: "p",
          message: { role: "assistant", content: "ok" }, usage: {}, latency_ms: 1
        )
      end
    )

    client.chat(model: "m", messages: "just a string")
    assert_equal [{ "role" => "user", "content" => "just a string" }], captured["messages"]
  end

  def test_chat_with_stream_true_raises_argument_error
    client = Llmshim::Client.new(base_url: "http://localhost:0", timeout: 5)
    err = assert_raises(ArgumentError) do
      client.chat(model: "m", messages: "hi", stream: true)
    end
    assert_match(/#stream/, err.message)
  end

  # --- POST /v1/chat/stream ------------------------------------------------

  SSE = <<~SSE
    event: reasoning
    data: {"text":"let me think"}

    event: content
    data: {"text":"Hello"}

    event: content
    data: {"text":" world"}

    event: tool_call
    data: {"id":"call_9","name":"lookup","arguments":"{\\"q\\":\\"x\\"}"}

    event: usage
    data: {"input_tokens":7,"output_tokens":11,"total_tokens":18}

    event: done
    data: {}

  SSE

  def stream_client
    client_for(
      "/v1/chat/stream" => lambda do |_req, res|
        res.content_type = "text/event-stream"
        res.body = SSE
      end
    )
  end

  def test_stream_yields_typed_events_and_stops_on_done
    events = []
    stream_client.stream(model: "m", messages: "hi") { |ev| events << ev }

    assert_equal %w[reasoning content content tool_call usage done], events.map(&:type)

    assert(events[0].reasoning?)
    assert_equal "let me think", events[0].text

    assert(events[1].content?)
    assert_equal "Hello", events[1].text
    assert_equal " world", events[2].text

    tc = events[3]
    assert(tc.tool_call?)
    assert_equal "call_9", tc.id
    assert_equal "lookup", tc.name
    assert_equal '{"q":"x"}', tc.arguments

    u = events[4]
    assert(u.usage?)
    assert_equal 18, u.usage.total_tokens

    assert(events.last.done?)
  end

  def test_stream_returns_collected_events_without_block
    events = stream_client.stream(model: "m", messages: "hi")
    assert_kind_of Array, events
    assert_equal 6, events.length
    assert_equal "content", events[1].type
  end

  def test_stream_reassembles_content_text
    text = stream_client.stream(model: "m", messages: "hi")
              .select(&:content?).map(&:text).join
    assert_equal "Hello world", text
  end

  # --- GET /v1/models ------------------------------------------------------

  def test_models
    client = client_for(
      "/v1/models" => lambda do |_req, res|
        res.content_type = "application/json"
        res.body = JSON.generate(
          models: [
            { id: "anthropic/claude-sonnet-5", provider: "anthropic", name: "claude-sonnet-5" },
            { id: "openai/gpt-5.5", provider: "openai", name: "gpt-5.5" }
          ]
        )
      end
    )

    models = client.models
    assert_equal 2, models.length
    assert_instance_of Llmshim::Model, models.first
    assert_equal "anthropic/claude-sonnet-5", models.first.id
    assert_equal "openai", models.last.provider
  end

  # --- GET /health ---------------------------------------------------------

  def test_health
    client = client_for(
      "/health" => lambda do |_req, res|
        res.content_type = "application/json"
        res.body = JSON.generate(status: "ok", providers: %w[anthropic openai])
      end
    )

    health = client.health
    assert_instance_of Llmshim::Health, health
    assert_equal "ok", health.status
    assert_equal %w[anthropic openai], health.providers
  end

  # --- error handling ------------------------------------------------------

  def test_non_2xx_raises_typed_api_error
    client = client_for(
      "/v1/chat" => lambda do |_req, res|
        res.status = 400
        res.content_type = "application/json"
        res.body = JSON.generate(error: { code: "unknown_provider", message: "no such model" })
      end
    )

    err = assert_raises(Llmshim::APIError) do
      client.chat(model: "bogus/model", messages: "hi")
    end

    assert_equal 400, err.status
    assert_equal "unknown_provider", err.code
    assert_equal "no such model", err.message
  end

  def test_non_json_error_body_falls_back_to_raw
    client = client_for(
      "/v1/chat" => lambda do |_req, res|
        res.status = 502
        res.content_type = "text/plain"
        res.body = "upstream boom"
      end
    )

    err = assert_raises(Llmshim::APIError) do
      client.chat(model: "m", messages: "hi")
    end
    assert_equal 502, err.status
    assert_nil err.code
    assert_equal "upstream boom", err.message
  end

  def test_stream_non_2xx_raises_api_error
    client = client_for(
      "/v1/chat/stream" => lambda do |_req, res|
        res.status = 400
        res.content_type = "application/json"
        res.body = JSON.generate(error: { code: "bad_request", message: "missing model" })
      end
    )

    err = assert_raises(Llmshim::APIError) do
      client.stream(model: "", messages: "hi") { |_ev| }
    end
    assert_equal 400, err.status
    assert_equal "bad_request", err.code
  end
end
