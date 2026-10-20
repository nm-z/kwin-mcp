#!/usr/bin/env ruby
# Verify returned and normalized KWin IDs through production stdio and real dialogs.
require 'fileutils'
require 'json'
require 'open3'
require 'shellwords'
require 'tmpdir'
require 'timeout'

binary, evidence = ARGV
abort 'usage: window_activation_ids_e2e.rb SERVER EVIDENCE' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)

class ActivationIdProbe
  def initialize(binary, home, evidence)
    @home, @evidence, @id = home, evidence, 0
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
    raise response.inspect if response['error'] || response.dig('result', 'isError')
    response.fetch('result')
  end

  def data(name, args = {})
    args = args.merge(session_id: @session) if @session
    rpc('tools/call', name: name, arguments: args).fetch('structuredContent')
  end

  def start
    rpc('initialize', protocolVersion: '2024-11-05', capabilities: {},
        clientInfo: {name: 'window-activation-ids', version: '1'})
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized')); @input.flush
    started = data('session_start', width: 1000, height: 700,
                   browser_profiles: 'fresh', window_mode: 'native', startup_timeout_seconds: 60)
    @session = started.fetch('session_id')
  end

  def canonical(id)
    id.strip.sub(/\A\{/, '').sub(/\}\z/, '').downcase
  end

  def windows
    data('window_list').fetch('windows')
  end

  def assert_active(id)
    active = windows.find { |window| window.fetch('active') } || (raise 'no active window')
    raise "wrong active window: #{active}" unless canonical(active.fetch('id')) == canonical(id)
    active
  end

  def run
    %w[first second].each do |name|
      output = "/tmp/activation-#{name}-497.txt"
      data('launch_app', command: "kdialog --title 'Activation #{name} 497' --inputbox 'Receipt #{name} 497' > #{Shellwords.escape(output)}")
    end
    listed = windows
    first, second = %w[first second].map do |name|
      listed.find { |window| window.fetch('title').include?("Activation #{name} 497") } || (raise "#{name} dialog missing")
    end
    raise 'fixture window IDs collide' if canonical(first.fetch('id')) == canonical(second.fetch('id'))
    records = []
    [[first, second], [second, first]].each do |target, other|
      id = target.fetch('id')
      plain = canonical(id)
      {listed: id, unbraced: plain, braced: "{#{plain}}", uppercase: "{#{plain.upcase}}"}.each do |variant, supplied|
        data('window_activate', window_id: other.fetch('id'))
        assert_active(other.fetch('id'))
        activated = data('window_activate', window_id: supplied)
        raise 'request identity was lost' unless activated.fetch('requested_window_id') == supplied
        raise 'activation returned another window' unless canonical(activated.fetch('window').fetch('id')) == plain
        observed = assert_active(id)
        records << {variant: variant, supplied_id: supplied, actual_id: observed.fetch('id'), title: observed.fetch('title')}
      end
    end
    # Export the app's accepted input, independently of the activation receipt.
    [[first, 'first497', 'first'], [second, 'second497', 'second']].each do |target, text, name|
      data('window_activate', window_id: "{#{canonical(target.fetch('id')).upcase}}")
      assert_active(target.fetch('id'))
      data('keyboard_type', text: text)
      capture = data('screenshot')
      raise 'screenshot names another window' unless canonical(capture.fetch('window').fetch('id')) == canonical(target.fetch('id'))
      FileUtils.cp(capture.fetch('path'), File.join(@evidence, "#{name}-focused.png"))
      data('keyboard_key', key: 'Return')
      path = File.join(@evidence, "#{name}-receipt.txt")
      data('export_file', session_path: "/tmp/activation-#{name}-497.txt", host_path: path)
      raise "#{name} received different input" unless File.binread(path) == "#{text}\n"
    end
    summary = {session_id: @session, activation_checks: records, independent_dialog_receipts: true, passed: true}
    File.write(File.join(@evidence, 'verification.json'), JSON.pretty_generate(summary))
    puts JSON.generate(summary)
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

Dir.mktmpdir('k497-', ENV.fetch('KWIN_MCP_TEST_TMPDIR', '/var/tmp')) do |home|
  probe = nil
  begin
    probe = ActivationIdProbe.new(binary, home, evidence)
    probe.start
    probe.run
  ensure
    probe&.close
  end
end
