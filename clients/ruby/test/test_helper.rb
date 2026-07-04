# frozen_string_literal: true

$LOAD_PATH.unshift File.expand_path("../lib", __dir__)

require "minitest/autorun"
require "webrick"
require "json"
require "llmshim"

# Spins up a local WEBrick server so tests exercise real HTTP parsing without
# ever contacting a provider or requiring a running proxy. Handlers are plain
# procs that receive (req, res) and return canned data.
class MockProxy
  attr_reader :port, :requests

  def initialize(routes)
    @routes = routes
    @requests = []
    @server = WEBrick::HTTPServer.new(
      BindAddress: "127.0.0.1",
      Port: 0, # ephemeral
      Logger: WEBrick::Log.new(File::NULL),
      AccessLog: []
    )
    @port = @server.listeners.first.addr[1]

    @routes.each do |path, handler|
      @server.mount_proc(path) do |req, res|
        @requests << { path: req.path, method: req.request_method, body: req.body }
        handler.call(req, res)
      end
    end

    @thread = Thread.new { @server.start }
    wait_until_ready
  end

  def base_url
    "http://127.0.0.1:#{@port}"
  end

  def shutdown
    @server.shutdown
    @thread.join
  end

  private

  def wait_until_ready
    20.times do
      TCPSocket.new("127.0.0.1", @port).close
      return
    rescue StandardError
      sleep 0.02
    end
  end
end
