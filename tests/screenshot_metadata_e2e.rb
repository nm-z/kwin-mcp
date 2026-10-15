#!/usr/bin/env ruby
# Exercise capture responses through the production stdio server on a KDE host.
# Usage: ruby tests/screenshot_metadata_e2e.rb /path/to/kwin-mcp /path/to/evidence
require 'base64'
require 'fileutils'
require 'json'
require 'open3'
require 'timeout'

binary, evidence = ARGV
abort 'usage: screenshot_metadata_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
private_home = File.join(evidence, 'home')
%w[.config .cache .local/share .local/state .kde].each do |directory|
  FileUtils.mkdir_p(File.join(private_home, directory))
end

def rpc(method, params)
  @request_id += 1
  @input.puts(JSON.generate(jsonrpc: '2.0', id: @request_id, method: method, params: params))
  @input.flush
  response = Timeout.timeout(90) do
    loop do
      line = @output.gets or raise 'unexpected server EOF'
      message = JSON.parse(line)
      break message if message['id'] == @request_id
    end
  end
  @trace.puts(JSON.generate(method: method, params: params, response: response))
  @trace.flush
  raise "MCP error: #{response}" if response['error'] || response.dig('result', 'isError')
  response.fetch('result')
end

def call(name, arguments = {})
  arguments = arguments.merge(session_id: @session_id) if @session_id
  rpc('tools/call', name: name, arguments: arguments)
end

def capture(evidence, tag, arguments, saved: true)
  # An input tool immediately before capture exercises optional settle metadata.
  call('mouse_move', x: 40, y: 40)
  result = call('screenshot', arguments)
  content = result.fetch('content')
  inline = arguments[:inline]
  metadata = result.fetch('structuredContent')
  raise 'missing settle metadata' unless metadata.fetch('settle').fetch('settled') == true
  raise 'missing settle duration' unless metadata.fetch('settle').fetch('waited_ms').is_a?(Integer)
  if inline
    raise 'inline content blocks changed' unless content.map { |block| block.fetch('type') } == %w[text text image]
    raise 'text and structured metadata differ' unless JSON.parse(content[1].fetch('text')) == metadata
    image = content[2]
    raise 'wrong inline MIME type' unless image.fetch('mimeType') == 'image/png'
    png = Base64.strict_decode64(image.fetch('data'))
  else
    raise 'file capture unexpectedly contains image content' if content.any? { |block| block['type'] == 'image' }
    png = File.binread(metadata.fetch('path'))
  end
  if saved
    raise 'saved PNG differs from inline PNG' unless png == File.binread(metadata.fetch('path'))
    raise 'unexpected write error' if metadata.key?('write_error')
  else
    raise 'failed save claims a path' unless metadata.fetch('path').nil?
    raise 'failed save omits error' if metadata.fetch('write_error').empty?
  end
  raise 'invalid PNG header' unless png.start_with?("\x89PNG\r\n\x1a\n".b) && png[12, 4] == 'IHDR'
  dimensions = png[16, 8].unpack('NN')
  raise 'PNG dimensions disagree with metadata' unless dimensions == metadata.values_at('width', 'height')
  x1, y1, x2, y2 = metadata.fetch('region')
  raise 'region dimensions disagree with PNG' unless dimensions == [x2 - x1, y2 - y1]
  File.binwrite(File.join(evidence, "#{tag}.png"), png)
  File.write(File.join(evidence, "#{tag}.json"), JSON.pretty_generate(metadata))
  puts "#{tag}=PASS (#{dimensions.join('x')}, structured metadata#{inline ? ', inline image' : ''})"
  metadata
end

environment = {
  'HOME' => private_home,
  'XDG_CONFIG_HOME' => File.join(private_home, '.config'),
  'XDG_DATA_HOME' => File.join(private_home, '.local/share'),
  'XDG_CACHE_HOME' => File.join(private_home, '.cache'),
  'XDG_STATE_HOME' => File.join(private_home, '.local/state'),
  'KDEHOME' => File.join(private_home, '.kde'),
  'XDG_RUNTIME_DIR' => "/run/user/#{Process.euid}",
  'DBUS_SESSION_BUS_ADDRESS' => "unix:path=/run/user/#{Process.euid}/bus",
  'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil,
  'KWIN_MCP_REPO' => File.join(evidence, 'no-source'), 'CARGO' => '/usr/bin/false',
}
File.open(File.join(evidence, 'rpc.jsonl'), 'w') do |trace|
  @trace = trace
  File.open(File.join(evidence, 'server.stderr'), 'w') do |diagnostic|
    Open3.popen3(environment, binary, '--autoclean', '--width', '1920', '--height', '1080') do |input, output, errors, process|
      @input, @output, @request_id = input, output, 0
      stderr_reader = Thread.new { IO.copy_stream(errors, diagnostic) }
      begin
        rpc('initialize', protocolVersion: '2024-11-05', capabilities: {}, clientInfo: {name: 'screenshot-metadata-e2e', version: '1'})
        input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); input.flush
        @session_id = call('session_start').fetch('structuredContent').fetch('session_id')
        call('launch_app', command: 'kdialog --title "Screenshot metadata fixture" --inputbox "Synthetic capture for issue #365"')
        windows = call('window_list').fetch('structuredContent').fetch('windows')
        window = windows.find { |item| item.fetch('title').include?('Screenshot metadata fixture') }
        raise 'private fixture window missing' unless window
        file = capture(evidence, 'file', {})
        raise 'capture names wrong window' unless file.fetch('window').fetch('id') == window.fetch('id')
        full = capture(evidence, 'inline-full', {inline: true})
        raise 'full inline metadata differs from file capture' unless full.reject { |key, _| key == 'settle' } == file.reject { |key, _| key == 'settle' }
        crop = capture(evidence, 'inline-crop', {inline: true, region: [0, 0, 120, 80]})
        raise 'requested crop not applied' unless crop.fetch('region') == [0, 0, 120, 80]
        capture(evidence, 'inline-cursor', {inline: true, cursor: true})
        # Force only this capture's atomic rename to fail, without filling /tmp
        # or changing permissions or quotas for another session.
        path = file.fetch('path')
        File.unlink(path)
        Dir.mkdir(path)
        begin
          capture(evidence, 'inline-write-failure', {inline: true}, saved: false)
          raise 'write failure left a partial PNG' unless File.directory?(path) && Dir.empty?(path)
        ensure
          Dir.rmdir(path)
        end
        capture(evidence, 'file-recovered', {})
      ensure
        call('session_stop') if @session_id
        input.close
        Timeout.timeout(30) { process.value }
        stderr_reader.join
      end
    end
  end
end
