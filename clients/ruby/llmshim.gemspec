# frozen_string_literal: true

require_relative "lib/llmshim/version"

Gem::Specification.new do |spec|
  spec.name = "llmshim"
  spec.version = Llmshim::VERSION
  spec.authors = ["Sanjay Nadhavajhala"]
  spec.email = ["sanjay@f2.ai"]

  spec.summary = "Ruby client for the llmshim multi-provider LLM proxy."
  spec.description = <<~DESC
    A thin, dependency-free HTTP client for a running llmshim proxy. Send
    OpenAI-style chat requests and let llmshim translate to OpenAI, Anthropic,
    Google Gemini, or xAI. Supports non-streaming and SSE streaming, model
    listing, and health checks. Standard library only.
  DESC
  spec.homepage = "https://github.com/sanjay920/llmshim"
  spec.license = "MIT"
  spec.required_ruby_version = ">= 2.6.0"

  spec.metadata["homepage_uri"] = spec.homepage
  spec.metadata["source_code_uri"] = "https://github.com/sanjay920/llmshim"
  spec.metadata["changelog_uri"] = "https://github.com/sanjay920/llmshim/releases"

  spec.files = Dir[
    "lib/**/*.rb",
    "README.md"
  ]
  spec.require_paths = ["lib"]

  # Runtime: standard library only (net/http, json, uri) — no runtime deps.

  spec.add_development_dependency "minitest", "~> 5.0"
  spec.add_development_dependency "rake", "~> 13.0"
end
