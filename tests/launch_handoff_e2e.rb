#!/usr/bin/env ruby
# Production-stdio checks for launch handoffs:
#   focus_gap   (#574) screenshot while one dialog has closed and the next is mapping
#   first_run   (#576) a fresh Chrome profile opens the requested URL, not first-run terms
#   hidden_path (#577) a host /tmp path names host_shared_dir, and a copy there runs
#   window_timeout (#254) a still-running command without a window fails; exit 0 and expect_window=false succeed
require 'fileutils'
require 'json'
require 'open3'
require 'shellwords'
require 'tmpdir'
require 'timeout'

binary, evidence, *cases = ARGV
abort 'usage: launch_handoff_e2e.rb SERVER EVIDENCE [focus_gap first_run hidden_path window_timeout]' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
cases = %w[focus_gap first_run hidden_path window_timeout] if cases.empty?

class LaunchHandoffProbe
  def initialize(binary, home, evidence)
    @evidence, @id = evidence, 0
    %w[.config .cache .local/share .local/state .kde].each { |path| FileUtils.mkdir_p(File.join(home, path)) }
    runtime = ENV.fetch('XDG_RUNTIME_DIR', "/run/user/#{Process.uid}")
    env = {
      'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config",
      'XDG_CACHE_HOME' => "#{home}/.cache", 'XDG_DATA_HOME' => "#{home}/.local/share",
      'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
      'XDG_RUNTIME_DIR' => runtime,
      'DBUS_SESSION_BUS_ADDRESS' => ENV.fetch('DBUS_SESSION_BUS_ADDRESS', "unix:path=#{runtime}/bus"),
      'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil, 'XAUTHORITY' => nil,
      'KWIN_MCP_REPO' => "#{home}/no-source", 'CARGO' => '/usr/bin/false'
    }
    @trace = File.open(File.join(evidence, 'rpc.jsonl'), 'w')
    @errors = File.open(File.join(evidence, 'server.stderr'), 'w')
    @input, @output, @process = Open3.popen2(env, binary, '--autoclean', err: @errors)
  end

  def rpc(method, params)
    @id += 1
    request = {jsonrpc: '2.0', id: @id, method: method, params: params}
    @trace.puts(JSON.generate(request: request)); @trace.flush
    @input.puts(JSON.generate(request)); @input.flush
    response = Timeout.timeout(90) do
      loop do
        value = JSON.parse(@output.gets || (raise 'server EOF'))
        break value if value['id'] == @id
      end
    end
    @trace.puts(JSON.generate(response: response)); @trace.flush
    response
  end

  # The raw JSON-RPC response, for calls whose error is the expected outcome.
  def call(name, args = {})
    args = args.merge(session_id: @session) if @session
    rpc('tools/call', name: name, arguments: args)
  end

  def data(name, args = {})
    response = call(name, args)
    raise "#{name} failed: #{response}" if response['error'] || response.dig('result', 'isError')
    response.dig('result', 'structuredContent')
  end

  def start
    response = rpc('initialize', protocolVersion: '2024-11-05', capabilities: {},
                                 clientInfo: {name: 'launch-handoff', version: '1'})
    raise response.inspect if response['error']
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); @input.flush
    started = data('session_start', width: 1280, height: 800, browser_profiles: 'fresh',
                                    startup_timeout_seconds: 60)
    @session = started.fetch('session_id')
    @shared = started.fetch('host_shared_dir')
  end

  def windows
    data('window_list').fetch('windows')
  end

  def wait_until(seconds, what)
    deadline = Time.now + seconds
    loop do
      value = yield
      return value if value
      raise "timed out after #{seconds}s waiting for #{what}" if Time.now > deadline
      sleep 0.1
    end
  end

  def save_screenshot(name)
    shot = data('screenshot')
    FileUtils.cp(shot.fetch('path'), File.join(@evidence, "#{name}.png"))
    shot
  end

  # A click or key that closes the only window leaves KWin with no active
  # window until the next one maps. screenshot must wait for it.
  def focus_gap
    command = 'kdialog --title kwin-574-first --msgbox first; sleep 0.4; ' \
              'kdialog --title kwin-574-second --msgbox second'
    launched = data('launch_app', command: command)
    raise "first dialog not reported: #{launched}" unless launched.dig('window_info', 'title').to_s.include?('kwin-574-first') ||
                                                         windows.any? { |window| window['title'].include?('kwin-574-first') }
    data('keyboard_key', key: 'Return')
    wait_until(10, 'the first dialog to close') { windows.none? { |window| window['title'].include?('kwin-574-first') } }
    gap = windows
    began = Time.now
    response = call('screenshot')
    elapsed = Time.now - began
    File.write(File.join(@evidence, 'focus-gap.json'), JSON.pretty_generate(windows_after_close: gap, screenshot: response, elapsed_s: elapsed))
    raise "screenshot during the focus gap failed: #{response}" if response['error'] || response.dig('result', 'isError')
    title = response.dig('result', 'structuredContent', 'window', 'title').to_s
    raise "screenshot captured #{title.inspect}, not the second dialog" unless title.include?('kwin-574-second')
    FileUtils.cp(response.dig('result', 'structuredContent', 'path'), File.join(@evidence, 'focus-gap-second.png'))
    data('keyboard_key', key: 'Return')
    wait_until(10, 'the second dialog to close') { windows.none? { |window| window['title'].include?('kwin-574-second') } }
    {active_windows_after_close: gap.count { |window| window['active'] }, screenshot_wait_s: elapsed.round(2), captured: title}
  end

  # A fresh profile must open the requested page, not Chrome's terms page.
  def first_run
    url = 'data:text/html,<title>kwin-576-requested</title><h1>requested page</h1>'
    data('launch_app', command: "google-chrome-stable #{Shellwords.escape(url)}", window_timeout_seconds: 30)
    titles = nil
    wait_until(20, 'the requested page title') do
      titles = windows.map { |window| window['title'] }
      titles.any? { |title| title.include?('kwin-576-requested') } ||
        titles.any? { |title| title.match?(/Terms of Service|Welcome to Google Chrome/) }
    end
    save_screenshot('first-run-window')
    raise "first-run window shown instead of the URL: #{titles}" if titles.any? { |title| title.match?(/Terms of Service|Welcome to Google Chrome/) }
    {window_titles: titles}
  end

  # A host /tmp path is reported with the shared directory that does work.
  def hidden_path
    Dir.mktmpdir('kwin-577-', '/tmp') do |host_dir|
      script = File.join(host_dir, 'hello.sh')
      File.write(script, "#!/bin/sh\nprintf ran > \"$1\"\n")
      File.chmod(0o755, script)
      response = call('launch_app', command: "#{script} #{@shared}/hidden-marker", expect_window: false)
      error = response['error'] || (raise "hidden host path was accepted: #{response}")
      details = error.fetch('data')
      raise "wrong reason: #{error}" unless details['reason'] == 'path_not_visible' && details['path'] == script
      raise "error does not name host_shared_dir #{@shared}: #{error}" unless details['host_shared_dir'] == @shared &&
                                                                             error['message'].include?(@shared)
      raise 'the hidden command ran' if File.exist?(File.join(@shared, 'hidden-marker'))
      shared_script = File.join(@shared, 'hello.sh')
      FileUtils.cp(script, shared_script)
      marker = File.join(@shared, 'shared-marker')
      data('launch_app', command: "#{shared_script} #{marker}", expect_window: false)
      wait_until(10, 'the shared script marker') { File.exist?(marker) && File.read(marker) == 'ran' }
      {error_message: error['message'], host_shared_dir: @shared}
    end
  end

  # A command still running with no window at the deadline is a failed window
  # launch, but it keeps running; exit 0 and expect_window=false still succeed.
  def window_timeout
    late = call('launch_app', command: 'sleep 3; kdialog --title kwin-254-late --msgbox late', window_timeout_seconds: 1)
    payload = late.dig('result', 'structuredContent') || (raise "timeout returned no result: #{late}")
    raise "timeout was not an error: #{late}" unless late.dig('result', 'isError') == true
    expected = {'window_outcome' => 'timeout', 'reason' => 'window_not_observed', 'command_submitted' => true, 'retry_safe' => false}
    expected.each { |key, value| raise "#{key} is #{payload[key].inspect}: #{late}" unless payload[key] == value }
    wait_until(15, 'the late dialog') { windows.any? { |window| window['title'].include?('kwin-254-late') } }
    count = windows.count { |window| window['title'].include?('kwin-254-late') }
    raise "the timed-out command ran #{count} times" unless count == 1
    data('keyboard_key', key: 'Return')
    exited = data('launch_app', command: "printf ok > #{@shared}/zero-exit")
    raise "zero exit without a window: #{exited}" unless exited['window_outcome'] == 'exited' && exited['exit_status'].zero?
    background = data('launch_app', command: 'sleep 2', expect_window: false)
    raise "expect_window=false: #{background}" unless background['window_outcome'] == 'not_expected'
    {timeout_text: late.dig('result', 'content', 0, 'text').to_s[0, 160], late_windows: count}
  end

  def close
    begin
      data('session_stop') if @session
    ensure
      @input&.close
      if @process
        begin
          status = Timeout.timeout(30) { @process.value }
        rescue Timeout::Error
          Process.kill('TERM', @process.pid) rescue Errno::ESRCH
          status = Timeout.timeout(30) { @process.value }
        end
        raise "server exit #{status.exitstatus}" unless status.success?
      end
      @output&.close; @errors&.close; @trace&.close
    end
  end
end

results = {}
Dir.mktmpdir('klh-', ENV.fetch('KWIN_MCP_TEST_TMPDIR', '/var/tmp')) do |home|
  probe = nil
  begin
    probe = LaunchHandoffProbe.new(binary, home, evidence)
    probe.start
    cases.each do |name|
      results[name] = {status: 'PASS', detail: probe.public_send(name)}
    rescue StandardError => error
      results[name] = {status: 'FAIL', detail: error.message[0, 2000]}
    end
  ensure
    probe&.close
  end
end
File.write(File.join(evidence, 'results.json'), JSON.pretty_generate(results))
results.each { |name, result| puts "#{result[:status]} #{name}: #{JSON.generate(result[:detail])[0, 400]}" }
exit(results.values.all? { |result| result[:status] == 'PASS' } ? 0 : 1)
