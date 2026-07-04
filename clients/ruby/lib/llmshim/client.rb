# frozen_string_literal: true

require "json"
require "net/http"
require "uri"

require_relative "errors"
require_relative "types"

module Llmshim
  # A thin, dependency-free HTTP client for a running llmshim proxy.
  #
  #   client = Llmshim::Client.new(base_url: "http://localhost:3000")
  #   resp = client.chat(model: "claude-sonnet-4-6", messages: "Hello!")
  #   puts resp.content
  #
  # All request/response shapes follow api/openapi.yaml. Only the Ruby
  # standard library is used (net/http, json, uri).
  class Client
    DEFAULT_BASE_URL = "http://localhost:3000"
    DEFAULT_TIMEOUT = 120

    # Base URL of the proxy (without a trailing slash).
    attr_reader :base_url
    # Extra headers sent with every request (Hash).
    attr_reader :headers
    # Read/open timeout in seconds.
    attr_reader :timeout

    # @param base_url [String] proxy base URL (default http://localhost:3000)
    # @param headers [Hash] extra headers merged into every request
    # @param timeout [Numeric] read/open timeout in seconds
    def initialize(base_url: DEFAULT_BASE_URL, headers: {}, timeout: DEFAULT_TIMEOUT)
      @base_url = base_url.to_s.sub(%r{/+\z}, "")
      @headers = headers || {}
      @timeout = timeout
    end

    # Send a non-streaming chat completion (POST /v1/chat).
    #
    # @param model [String] "provider/model" or a bare model name
    # @param messages [String, Array<Hash>] a single user string or message hashes
    # @param opts [Hash] see #build_body (max_tokens, temperature, top_p, top_k,
    #   stop, reasoning_effort, config, provider_config, tools, tool_choice, fallback)
    # @return [Llmshim::ChatResponse]
    def chat(model:, messages:, **opts)
      body = build_body(model, messages, opts)
      hash = post_json("/v1/chat", body)
      ChatResponse.from_hash(hash)
    end

    # Stream a chat completion (POST /v1/chat/stream).
    #
    # Yields a Llmshim::StreamEvent per SSE event. Iteration stops after a
    # "done" event or on EOF. When no block is given, returns the collected
    # array of events.
    #
    # @return [Array<Llmshim::StreamEvent>, nil]
    def stream(model:, messages:, **opts)
      body = build_body(model, messages, opts)
      collected = block_given? ? nil : []
      stream_sse("/v1/chat/stream", body) do |event|
        if block_given?
          yield event
        else
          collected << event
        end
      end
      collected
    end

    # List available models (GET /v1/models).
    #
    # @return [Array<Llmshim::Model>]
    def models
      hash = get_json("/v1/models")
      (hash["models"] || []).map { |m| Model.from_hash(m) }
    end

    # Health check (GET /health).
    #
    # @return [Llmshim::Health]
    def health
      Health.from_hash(get_json("/health"))
    end

    private

    # Build the JSON request body from friendly keyword options.
    #
    # config-level opts (max_tokens, temperature, top_p, top_k, stop,
    # reasoning_effort) are folded into +config+. tools / tool_choice are folded
    # into +provider_config+ (the proxy passes them through to the provider).
    def build_body(model, messages, opts)
      msgs =
        if messages.is_a?(String)
          [{ "role" => "user", "content" => messages }]
        else
          messages
        end

      body = { "model" => model, "messages" => msgs }

      config = stringify(opts[:config] || {})
      %i[max_tokens temperature top_p top_k stop reasoning_effort].each do |key|
        config[key.to_s] = opts[key] unless opts[key].nil?
      end
      body["config"] = config unless config.empty?

      provider_config = stringify(opts[:provider_config] || {})
      provider_config["tools"] = opts[:tools] unless opts[:tools].nil?
      provider_config["tool_choice"] = opts[:tool_choice] unless opts[:tool_choice].nil?
      body["provider_config"] = provider_config unless provider_config.empty?

      body["fallback"] = opts[:fallback] unless opts[:fallback].nil?
      body["stream"] = opts[:stream] unless opts[:stream].nil?

      body
    end

    def stringify(hash)
      hash.each_with_object({}) { |(k, v), acc| acc[k.to_s] = v }
    end

    def get_json(path)
      req = Net::HTTP::Get.new(uri_for(path))
      apply_headers(req)
      parse_json_response(perform(req))
    end

    def post_json(path, body)
      req = Net::HTTP::Post.new(uri_for(path))
      req["Content-Type"] = "application/json"
      apply_headers(req)
      req.body = JSON.generate(body)
      parse_json_response(perform(req))
    end

    # Perform a streaming POST and yield typed StreamEvent objects.
    def stream_sse(path, body)
      uri = uri_for(path)
      req = Net::HTTP::Post.new(uri)
      req["Content-Type"] = "application/json"
      req["Accept"] = "text/event-stream"
      apply_headers(req)
      req.body = JSON.generate(body)

      with_http(uri) do |http|
        http.request(req) do |res|
          unless res.is_a?(Net::HTTPSuccess)
            raise APIError.from_response(res.code.to_i, res.body)
          end

          # State carried across chunk boundaries. We read the full (finite)
          # SSE body rather than aborting mid-stream — abandoning net/http's
          # read_body leaves the socket in an inconsistent state. Once a "done"
          # event is seen we simply stop forwarding further events.
          state = { buffer: +"", event: nil, done: false }
          res.read_body do |chunk|
            next if state[:done]

            state[:buffer] << chunk
            while (idx = state[:buffer].index("\n"))
              line = state[:buffer].slice!(0..idx).chomp
              dispatch_sse_line(line, state) { |ev| yield ev }
              break if state[:done]
            end
          end
          # Flush any trailing line that arrived without a newline.
          unless state[:done] || state[:buffer].empty?
            dispatch_sse_line(state[:buffer].chomp, state) { |ev| yield ev }
          end
        end
      end
    end

    # Process one SSE line, updating +state+ in place and yielding a
    # StreamEvent for each complete data line until a "done" event is seen.
    def dispatch_sse_line(line, state)
      return if state[:done]
      return if line.empty?
      return if line.start_with?(":") # SSE comment / heartbeat

      if line.start_with?("event:")
        state[:event] = line.sub(/\Aevent:\s?/, "")
      elsif line.start_with?("data:")
        data = line.sub(/\Adata:\s?/, "")
        return if data.empty?

        parsed =
          begin
            JSON.parse(data)
          rescue JSON::ParserError
            {}
          end
        type = state[:event] || parsed["type"] || ""
        event = StreamEvent.new(type, parsed)
        yield event
        state[:done] = true if event.done?
      end
    end

    def uri_for(path)
      URI.parse("#{@base_url}#{path}")
    end

    def apply_headers(req)
      @headers.each { |k, v| req[k.to_s] = v }
    end

    def with_http(uri)
      http = Net::HTTP.new(uri.host, uri.port)
      http.use_ssl = uri.scheme == "https"
      http.open_timeout = @timeout
      http.read_timeout = @timeout
      http.start unless http.started?
      begin
        yield http
      ensure
        http.finish if http.started?
      end
    end

    def perform(req)
      uri = URI.parse("#{@base_url}#{req.path}")
      with_http(uri) { |http| http.request(req) }
    end

    def parse_json_response(res)
      unless res.is_a?(Net::HTTPSuccess)
        raise APIError.from_response(res.code.to_i, res.body)
      end

      body = res.body.to_s
      return {} if body.empty?

      JSON.parse(body)
    rescue JSON::ParserError => e
      raise Error, "Failed to parse response JSON: #{e.message}"
    end
  end
end
