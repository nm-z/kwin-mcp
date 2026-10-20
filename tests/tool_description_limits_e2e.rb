#!/usr/bin/env ruby
# Verify the text clients actually receive through the production stdio server.
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'

server, evidence = ARGV
abort 'usage: tool_description_limits_e2e.rb SERVER EVIDENCE' unless server && evidence
server = File.realpath(server)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
results = []
[false, true].each do |locked|
  args = ['--width', '3840', '--height', '2160', '--autoclean', '--no-viewer']
  args << '--no-override' if locked
  env = {'KWIN_MCP_REPO' => File.join(evidence, 'no-source'), 'CARGO' => '/usr/bin/false'}
  input, output, errors, process = Open3.popen3(env, server, *args)
  stderr = Thread.new { errors.read }
  id = 0
  call = lambda do |method, params|
    id += 1
    input.puts(JSON.generate(jsonrpc: '2.0', id: id, method: method, params: params))
    input.flush
    Timeout.timeout(10) do
      loop do
        reply = JSON.parse(output.gets || raise('server EOF'))
        next unless reply['id'] == id
        raise reply.inspect if reply['error']
        break reply.fetch('result')
      end
    end
  end
  begin
    info = call.call('initialize', {
      protocolVersion: '2024-11-05', capabilities: {},
      clientInfo: {name: 'tool-description-limits-e2e', version: '1'}
    })
    input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
    input.flush
    listed = call.call('tools/list', {})
    counts = listed.fetch('tools').to_h do |tool|
      [tool.fetch('name'), tool.fetch('description').length]
    end
    counts['initialize.instructions'] = info.fetch('instructions').length
    overflow = counts.select { |_, length| length > 2048 }
    raise "client text exceeds 2048 characters: #{overflow}" unless overflow.empty?
    raise 'empty instruction or tool description' if counts.values.any?(&:zero?)
    results << {locked: locked, counts: counts}
    puts JSON.generate(locked: locked, tool_count: listed.fetch('tools').length,
                       longest: counts.sort_by { |_, length| -length }.first(5))
  ensure
    input.close unless input.closed?
    begin
      Timeout.timeout(10) { process.value }
    rescue Timeout::Error
      Process.kill('TERM', process.pid) if process.alive?
      Timeout.timeout(10) { process.value }
    end
    File.write(File.join(evidence, "server-#{locked}.stderr"), stderr.value)
    output.close
    errors.close
  end
end
File.write(File.join(evidence, 'live-text-budget.json'), JSON.pretty_generate(results))
