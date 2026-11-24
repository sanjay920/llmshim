# frozen_string_literal: true

module Llmshim
  # Base class for all errors raised by this gem.
  class Error < StandardError; end

  # Raised when the proxy returns a non-2xx response.
  #
  # The proxy encodes errors as an +ErrorResponse+ object
  # (+{ "error": { "code": ..., "message": ... } }+). When the body cannot
  # be parsed, +code+ falls back to +nil+ and +message+ to the raw body.
  class APIError < Error
    # HTTP status code of the failed response (Integer).
    attr_reader :status
    # Machine-readable error code from the proxy (String or nil).
    attr_reader :code
    # Raw response body (String).
    attr_reader :body

    def initialize(message, status:, code: nil, body: nil)
      @status = status
      @code = code
      @body = body
      super(message)
    end

    # Build an APIError from an HTTP status and response body.
    def self.from_response(status, body)
      code = nil
      message = nil
      begin
        parsed = JSON.parse(body.to_s)
        err = parsed["error"]
        if err.is_a?(Hash)
          code = err["code"]
          message = err["message"]
        end
      rescue JSON::ParserError
        # fall through to raw body
      end
      message ||= (body.to_s.empty? ? "HTTP #{status}" : body.to_s)
      new(message, status: status, code: code, body: body)
    end
  end
end
