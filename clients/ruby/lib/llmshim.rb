# frozen_string_literal: true

require_relative "llmshim/version"
require_relative "llmshim/errors"
require_relative "llmshim/types"
require_relative "llmshim/client"

# Ruby client for the llmshim multi-provider LLM proxy.
#
# Point it at a running proxy (default http://localhost:3000) and send
# OpenAI-style chat requests; llmshim translates to the native provider API.
#
# Object API:
#
#   client = Llmshim::Client.new(base_url: "http://localhost:3000")
#   resp = client.chat(model: "claude-sonnet-4-6", messages: "Hello!")
#   puts resp.content
#
# Module convenience API (uses a shared default client):
#
#   Llmshim.chat(model: "gpt-5.5", messages: "Hello!")
#   Llmshim.stream(model: "gpt-5.5", messages: "Hi") { |ev| print ev.text if ev.content? }
#
module Llmshim
  class << self
    # Base URL used by the module-level convenience methods.
    # Defaults to the LLMSHIM_BASE_URL env var, then http://localhost:3000.
    attr_writer :base_url

    def base_url
      @base_url ||= ENV.fetch("LLMSHIM_BASE_URL", Client::DEFAULT_BASE_URL)
    end

    # The shared default Client used by the convenience methods.
    def default_client
      @default_client ||= Client.new(base_url: base_url)
    end

    # Replace the shared default client (e.g. to pass custom headers/timeout).
    attr_writer :default_client

    # @see Client#chat
    def chat(**kwargs, &block)
      default_client.chat(**kwargs, &block)
    end

    # @see Client#stream
    def stream(**kwargs, &block)
      default_client.stream(**kwargs, &block)
    end

    # @see Client#models
    def models
      default_client.models
    end

    # @see Client#health
    def health
      default_client.health
    end
  end
end
