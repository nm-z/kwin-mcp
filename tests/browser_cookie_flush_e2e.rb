#!/usr/bin/env ruby
# Production stdio + actual Chrome; all profile writes are under a private HOME.
require 'json'
require 'open3'
require 'fileutils'
require 'tmpdir'
require 'socket'
require 'timeout'
require 'base64'

binary = File.expand_path(ENV.fetch('KWIN_MCP_E2E_SERVER'))
evidence = File.expand_path(ENV.fetch('KWIN_MCP_TEST_ARTIFACT_DIR'))
FileUtils.mkdir_p(evidence)
fixture = Dir.mktmpdir('kwin-cookie-flush-', File.join(ENV.fetch('HOME'), '.cache'))
profile = File.join(fixture, '.config/google-chrome')
FileUtils.mkdir_p(File.join(profile, 'Default'))
File.write(File.join(profile, 'Default/Preferences'), JSON.generate(session: { restore_on_startup: 1 }))
baseline = Dir.glob(File.join(profile, '**/*')).select { |path| File.file?(path) }.to_h { |path| [path, File.binread(path)] }
requests = Queue.new
http = TCPServer.new('0.0.0.0', 0)
port = http.addr[1]
server = Thread.new do
  loop do
    connection = http.accept
    connection.setsockopt(Socket::IPPROTO_TCP, Socket::TCP_NODELAY, 1)
    lines = []
    Timeout.timeout(5) do
      while (line = connection.gets) && line != "\r\n"
        lines << line
      end
    end
    route = lines.first.to_s.split[1].to_s
    cookie = lines.find { |line| line.downcase.start_with?('cookie:') }.to_s.strip
    value = route.delete_prefix('/set/')
    setting = route.start_with?('/set/')
    present = cookie.include?('kwin_immediate=')
    body = "<!doctype html><html><title>Cookie continuity</title><body style='font:32px sans-serif'><h1>#{setting ? "SET #{value}" : (present ? 'COOKIE PRESENT' : 'COOKIE MISSING')}</h1><p>Private real Chrome cookie persistence test</p></body></html>"
    headers = setting ? "Set-Cookie: kwin_immediate=#{value}; Max-Age=3600; Path=/; HttpOnly\r\nSet-Cookie: kwin_session=#{value}; Path=/; HttpOnly\r\n" : ''
    connection.write("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: #{body.bytesize}\r\n#{headers}Connection: close\r\n\r\n#{body}")
    connection.close
    requests << [route, cookie, Process.clock_gettime(Process::CLOCK_MONOTONIC)] unless route == '/favicon.ico'
  rescue EOFError, Errno::EPIPE, Timeout::Error
    connection&.close
  end
end

environment = {
  'HOME' => fixture, 'XDG_CONFIG_HOME' => File.join(fixture, '.config'),
  'XDG_CACHE_HOME' => File.join(fixture, '.cache'),
  'XDG_DATA_HOME' => File.join(fixture, '.local/share'),
  'XDG_STATE_HOME' => File.join(fixture, '.local/state'),
  'KWIN_MCP_REPO' => File.join(fixture, 'no-repository'),
  'XDG_RUNTIME_DIR' => '/run/user/1000',
  'DBUS_SESSION_BUS_ADDRESS' => 'unix:path=/run/user/1000/bus',
  'WAYLAND_DISPLAY' => ENV.fetch('WAYLAND_DISPLAY', 'wayland-0')
}
environment.values_at('HOME', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'KWIN_MCP_REPO').each { |path| FileUtils.mkdir_p(path) }
input, output, errors, process = Open3.popen3(environment, binary, '--autoclean', '--no-viewer')
stderr = Thread.new { File.open(File.join(evidence, 'browser-flush.stderr.log'), 'w') { |log| errors.each_line { |line| log.write(line); log.flush } } }
serial = 0
live = nil
rpc = lambda do |method, params|
  serial += 1
  input.puts(JSON.generate(jsonrpc: '2.0', id: serial, method: method, params: params))
  input.flush
  response = Timeout.timeout(60) do
    loop do
      message = JSON.parse(output.gets || raise('MCP server closed stdout'))
      break message if message['id'] == serial
    end
  end
  raise response.inspect if response['error'] || response.dig('result', 'isError')
  response.fetch('result')
end
tool = lambda { |name, args = {}| rpc.call('tools/call', { name: name, arguments: args.merge(live ? { session_id: live } : {}) }) }
screenshot = lambda do |name|
  result = tool.call('screenshot', { inline: true })
  data = result.fetch('content').find { |item| item['type'] == 'image' }.fetch('data')
  File.binwrite(File.join(evidence, name), Base64.strict_decode64(data))
end
wait_route = lambda do |expected|
  Timeout.timeout(15) do
    loop do
      request = requests.pop
      break request if request.first == expected
    end
  end
end
begin
  rpc.call('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'real-cookie-flush', version: '1' } })
  input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
  input.flush
  3.times do |cycle|
    started = tool.call('session_start', { width: 1000, height: 700 })
    live = started.fetch('structuredContent').fetch('session_id')
    route = cycle.zero? ? '/set/0' : '/check'
    tool.call('launch_app', { command: "google-chrome-stable --password-store=basic --no-first-run --disable-background-mode http://127.0.0.1:#{port}#{route}" })
    request = wait_route.call(route)
    if cycle.positive?
      expected = ["kwin_immediate=#{cycle - 1}", "kwin_session=#{cycle - 1}"]
      raise "cookie lost after immediate stop: #{request.inspect}" unless expected.all? { |cookie| request[1].include?(cookie) }
      screenshot.call("browser-cookie-restored-#{cycle}.png")
      route = "/set/#{cycle}"
      tool.call('keyboard_key', { key: 'ctrl+l' })
      tool.call('keyboard_type', { text: "http://127.0.0.1:#{port}#{route}" })
      tool.call('keyboard_key', { key: 'Return' })
      request = wait_route.call(route)
    end
    screenshot.call("browser-cookie-set-#{cycle}.png")
    before_stop = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    raise 'test waited past browser batch flush interval' if before_stop - request[2] > 10
    tool.call('session_stop')
    live = nil
    puts "cycle #{cycle}: session_stop requested #{(before_stop - request[2]).round(3)}s after Set-Cookie response"
    jars = Dir.glob(File.join(fixture, '.local/state/kwin-mcp/browser-cookies/.config/google-chrome/Default/**/Cookies'))
    raise 'no private persisted Chrome jar' if jars.empty?
    changed = Dir.glob(File.join(profile, '**/*')).select { |path| File.file?(path) }.to_h { |path| [path, File.binread(path)] }
    raise 'server wrote fixture lower browser profile' unless baseline == changed
  end
  puts 'PASS: real Chrome cookie restored twice across immediate session_stop, host fixture unchanged'
ensure
  begin
    tool.call('session_stop') if live
  rescue StandardError => error
    warn "cleanup: #{error}"
  end
  begin
    input.close unless input.closed?
    begin
      Timeout.timeout(45) { process.value }
    rescue Timeout::Error
      # The server owns this test's sandbox. Give its shutdown handler another
      # chance before killing the owned server; never leave a test running.
      Process.kill('TERM', process.pid) if process.alive?
      begin
        Timeout.timeout(45) { process.value }
      rescue Timeout::Error
        Process.kill('KILL', process.pid) if process.alive?
        process.value
        raise 'test server did not finish session cleanup'
      end
    end
  ensure
    stderr.join unless process.alive?
    server.kill
    server.join
    http.close
    FileUtils.remove_entry(fixture) unless process.alive?
  end
end
