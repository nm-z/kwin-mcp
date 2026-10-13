#!/usr/bin/env ruby
# Production stdio + actual Chrome; all profile writes are under a private HOME.
require 'json'
require 'open3'
require 'fileutils'
require 'tmpdir'
require 'timeout'
require 'base64'

binary = File.expand_path(ENV.fetch('KWIN_MCP_E2E_SERVER'))
evidence = File.expand_path(ENV.fetch('KWIN_MCP_TEST_ARTIFACT_DIR'))
test_bus = ENV.fetch('KWIN_MCP_TEST_DBUS_SESSION_BUS_ADDRESS')
raise 'cookie session checks require a filtered test D-Bus proxy' if test_bus.include?('/run/user/1000/bus')
FileUtils.mkdir_p(evidence)
fixture = Dir.mktmpdir('kwin-cookie-flush-', File.join(ENV.fetch('HOME'), '.cache'))
profile = File.join(fixture, '.config/google-chrome')
FileUtils.mkdir_p(File.join(profile, 'Default'))
File.write(File.join(profile, 'Default/Preferences'), JSON.generate(session: { restore_on_startup: 1 }))
baseline = Dir.glob(File.join(profile, '**/*')).select { |path| File.file?(path) }.to_h { |path| [path, File.binread(path)] }
request_log = File.join(fixture, 'cookie-requests.jsonl')
exported_log = File.join(fixture, 'exported-cookie-requests.jsonl')
server_log = File.join(fixture, 'cookie-http-server.log')
exported_server_log = File.join(fixture, 'exported-cookie-http-server.log')
ready_log = File.join(fixture, 'cookie-http-ready')
exported_ready_log = File.join(fixture, 'exported-cookie-http-ready')
cookie_counts_log = File.join(fixture, 'cookie-counts.json')
exported_cookie_counts_log = File.join(fixture, 'exported-cookie-counts.json')
File.write(request_log, '')
File.write(server_log, '')
FileUtils.cp(File.join(__dir__, 'cookie_http_server.rb'), File.join(fixture, 'cookie_http_server.rb'))

environment = {
  'HOME' => fixture, 'XDG_CONFIG_HOME' => File.join(fixture, '.config'),
  'XDG_CACHE_HOME' => File.join(fixture, '.cache'),
  'XDG_DATA_HOME' => File.join(fixture, '.local/share'),
  'XDG_STATE_HOME' => File.join(fixture, '.local/state'),
  'KWIN_MCP_REPO' => File.join(fixture, 'no-repository'),
  'XDG_RUNTIME_DIR' => '/run/user/1000',
  'DBUS_SESSION_BUS_ADDRESS' => test_bus,
  'WAYLAND_DISPLAY' => ENV.fetch('WAYLAND_DISPLAY', 'wayland-0')
}
environment.values_at('HOME', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'KWIN_MCP_REPO').each { |path| FileUtils.mkdir_p(path) }
input, output, errors, process = Open3.popen3(environment, binary, '--autoclean', '--no-viewer')
server_stderr = File.join(evidence, 'browser-flush.stderr.log')
stderr = Thread.new { File.open(server_stderr, 'w') { |log| errors.each_line { |line| log.write(line); log.flush } } }
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
      tool.call('export_file', { session_path: request_log, host_path: exported_log, overwrite: true })
      records = File.readlines(exported_log).filter_map do |line|
        JSON.parse(line)
      rescue JSON::ParserError
        nil
      end
      request = records.reverse.find { |record| record['route'] == expected }
      break [request, Process.clock_gettime(Process::CLOCK_MONOTONIC)] if request
      Thread.pass
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
    expected = cycle.zero? ? '-' : (cycle - 1).to_s
    tool.call('launch_app', { command: "ruby \"$HOME/cookie_http_server.rb\" 18765 \"$HOME/cookie-requests.jsonl\" #{expected}" })
    tool.call('export_file', { session_path: ready_log, host_path: exported_ready_log, overwrite: true })
    tool.call('export_file', { session_path: cookie_counts_log, host_path: exported_cookie_counts_log, overwrite: true })
    cookie_counts = JSON.parse(File.read(exported_cookie_counts_log))
    puts "cycle #{cycle}: merged private cookie rows #{cookie_counts.map { |entry| entry['rows'] }}"
    if cycle.positive? && !cookie_counts.any? { |entry| entry['rows'] == 2 }
      raise 'session_start did not merge both saved cookies into the private browser profile'
    end
    route = cycle.zero? ? '/set/0' : '/check'
    launched = tool.call('launch_app', { command: "google-chrome-stable --password-store=basic --no-first-run --disable-background-mode http://127.0.0.1:18765#{route}" })
    puts "cycle #{cycle}: launch window #{JSON.generate(launched.dig('structuredContent', 'window'))}"
    begin
      request = wait_route.call(route)
    rescue Timeout::Error
      screenshot.call("browser-cookie-timeout-#{cycle}.png")
      tool.call('export_file', { session_path: server_log, host_path: exported_server_log, overwrite: true })
      warn "cookie server log: #{File.read(exported_server_log)}"
      windows = tool.call('window_list')
      warn "windows after HTTP timeout: #{JSON.generate(windows.dig('structuredContent', 'windows'))}"
      raise
    end
    set_response_time = nil
    if cycle.positive?
      screenshot.call("browser-cookie-restored-#{cycle}.png")
      unless request[0]['immediate'] && request[0]['session']
        raise "cookie continuity failed in HTTP probe: #{request[0].inspect}"
      end
      route = "/set/#{cycle}"
      tool.call('keyboard_key', { key: 'ctrl+l' })
      tool.call('keyboard_type', { text: "http://127.0.0.1:18765#{route}" })
      tool.call('keyboard_key', { key: 'Return' })
      request = wait_route.call(route)
      set_response_time = request[1]
    else
      screenshot.call('browser-cookie-set-0.png')
      set_response_time = request[1]
      tool.call('keyboard_key', { key: 'ctrl+l' })
      tool.call('keyboard_type', { text: 'http://127.0.0.1:18765/check' })
      tool.call('keyboard_key', { key: 'Return' })
      request = wait_route.call('/check')
      screenshot.call('browser-cookie-restored-0.png')
      unless request[0]['immediate'] && request[0]['session']
        raise "cookies were not sent within the first session: #{request[0].inspect}"
      end
    end
    screenshot.call("browser-cookie-set-#{cycle}.png") unless cycle.zero?
    before_stop = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    raise 'test waited past browser batch flush interval' if before_stop - set_response_time > 10
    tool.call('session_stop')
    live = nil
    puts "cycle #{cycle}: session_stop requested #{(before_stop - set_response_time).round(3)}s after Set-Cookie response"
    shutdown_deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 5
    shutdown_lines = []
    loop do
      shutdown_lines = File.readlines(server_stderr).grep(/teardown: graceful browser shutdown requested/)
      break if shutdown_lines.length >= cycle + 1
      raise 'session_stop did not report graceful browser shutdown' if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= shutdown_deadline
      Thread.pass
    end
    shutdown_count = Integer(shutdown_lines[cycle][/for (\d+) Chromium/, 1])
    raise 'session_stop did not detect the private Chrome browser' if shutdown_count.zero?
    shutdown_completed = []
    completion_deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 5
    loop do
      shutdown_completed = File.readlines(server_stderr).grep(/teardown: graceful browser shutdown completed/)
      break if shutdown_completed.length >= cycle + 1
      raise 'Chrome did not finish graceful shutdown' if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= completion_deadline
      Thread.pass
    end
    jars = Dir.glob(File.join(fixture, '.local/state/kwin-mcp/browser-cookies/.config/google-chrome/Default/**/Cookies'))
    raise 'no private persisted Chrome jar' if jars.empty?
    saved_counts = jars.map do |jar|
      output, status = Open3.capture2(
        'sqlite3', jar,
        "SELECT count(*) FROM cookies WHERE name IN ('kwin_immediate','kwin_session')"
      )
      raise 'could not count synthetic persisted cookie rows' unless status.success?
      Integer(output.strip)
    end
    puts "cycle #{cycle}: persisted synthetic cookie row counts #{saved_counts.inspect}"
    raise 'Chrome did not persist both synthetic cookies' unless saved_counts.include?(2)
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
    FileUtils.remove_entry(fixture) unless process.alive?
  end
end
