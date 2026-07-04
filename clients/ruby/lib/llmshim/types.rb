# frozen_string_literal: true

module Llmshim
  # Token accounting returned with a chat response or a usage stream event.
  #
  # Mirrors the +Usage+ schema in api/openapi.yaml.
  Usage = Struct.new(
    :input_tokens, :output_tokens, :reasoning_tokens, :total_tokens,
    keyword_init: true
  ) do
    def self.from_hash(hash)
      return nil if hash.nil?

      new(
        input_tokens: hash["input_tokens"],
        output_tokens: hash["output_tokens"],
        reasoning_tokens: hash["reasoning_tokens"],
        total_tokens: hash["total_tokens"]
      )
    end
  end

  # A single tool call requested by the assistant.
  #
  # +function+ is a plain Hash with "name" and "arguments" (JSON-encoded string)
  # to match the wire format exactly.
  ToolCall = Struct.new(:id, :type, :function, keyword_init: true) do
    def self.from_hash(hash)
      new(id: hash["id"], type: hash["type"], function: hash["function"])
    end

    # Convenience accessor for the tool name.
    def name
      function && function["name"]
    end

    # Convenience accessor for the JSON-encoded arguments string.
    def arguments
      function && function["arguments"]
    end
  end

  # The assistant message inside a ChatResponse.
  ResponseMessage = Struct.new(:role, :content, :tool_calls, keyword_init: true) do
    def self.from_hash(hash)
      calls = (hash["tool_calls"] || []).map { |c| ToolCall.from_hash(c) }
      new(role: hash["role"], content: hash["content"], tool_calls: calls)
    end
  end

  # A non-streaming chat completion response (+ChatResponse+ schema).
  #
  # +raw+ retains the original parsed Hash for forward compatibility.
  ChatResponse = Struct.new(
    :id, :model, :provider, :message, :reasoning, :usage, :latency_ms, :raw,
    keyword_init: true
  ) do
    def self.from_hash(hash)
      new(
        id: hash["id"],
        model: hash["model"],
        provider: hash["provider"],
        message: ResponseMessage.from_hash(hash["message"] || {}),
        reasoning: hash["reasoning"],
        usage: Usage.from_hash(hash["usage"]),
        latency_ms: hash["latency_ms"],
        raw: hash
      )
    end

    # Shortcut for the assistant text content.
    def content
      message&.content
    end
  end

  # A single model entry from GET /v1/models.
  Model = Struct.new(:id, :provider, :name, keyword_init: true) do
    def self.from_hash(hash)
      new(id: hash["id"], provider: hash["provider"], name: hash["name"])
    end
  end

  # The GET /health response.
  Health = Struct.new(:status, :providers, keyword_init: true) do
    def self.from_hash(hash)
      new(status: hash["status"], providers: hash["providers"] || [])
    end
  end

  # A typed SSE event from POST /v1/chat/stream.
  #
  # +type+ is one of: "content", "reasoning", "tool_call", "usage", "done",
  # "error". All variant fields are exposed through +raw+ and the helper
  # accessors below; unknown fields remain reachable via +raw+.
  class StreamEvent
    # Event type string (String).
    attr_reader :type
    # Original parsed data Hash for this event.
    attr_reader :raw

    def initialize(type, raw)
      @type = type
      @raw = raw || {}
    end

    def content?
      type == "content"
    end

    def reasoning?
      type == "reasoning"
    end

    def tool_call?
      type == "tool_call"
    end

    def usage?
      type == "usage"
    end

    def done?
      type == "done"
    end

    def error?
      type == "error"
    end

    # content / reasoning events
    def text
      raw["text"]
    end

    # tool_call events
    def id
      raw["id"]
    end

    def name
      raw["name"]
    end

    def arguments
      raw["arguments"]
    end

    # usage events
    def usage
      return nil unless usage?

      Usage.from_hash(raw)
    end

    # error events
    def message
      raw["message"]
    end

    def [](key)
      raw[key.to_s]
    end
  end
end
