#!/usr/bin/env ruby
# Run with private, already-built binaries on the KDE host. The outer stdio
# session provides an isolated KWin desktop for
# the viewer; a second production stdio session supplies the picture and input.
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'shellwords'

class Rpc
  attr_reader :home, :session, :workdir

  def initialize(binary, home, log, extra_env = {})
    @home = home
    %w[.config .local/share .cache .local/state .kde].each { |p| FileUtils.mkdir_p(File.join(home, p)) }
    tmpdir = ENV.fetch('KWIN_MCP_E2E_TMPDIR', '/tmp')
    FileUtils.mkdir_p(tmpdir)
    runtime = "/run/user/#{Process.uid}"
    env = { 'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config", 'XDG_DATA_HOME' => "#{home}/.local/share",
            'XDG_CACHE_HOME' => "#{home}/.cache", 'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
            'KWIN_MCP_REPO' => "#{home}/no-repository", 'KDE_SESSION_VERSION' => '6',
            'TMPDIR' => tmpdir,
            'XDG_RUNTIME_DIR' => runtime, 'DBUS_SESSION_BUS_ADDRESS' => "unix:path=#{runtime}/bus",
            'WAYLAND_DISPLAY' => 'wayland-0' }
    @input, @output, error, @wait = Open3.popen3(env.merge(extra_env), binary, '--autoclean')
    @error_reader = Thread.new { File.open(log, 'w') { |f| error.each_line { |line| f.write(line); f.flush } } }
    @id = 0
    @rpc_log = File.open("#{log}.rpc", 'w')
    request('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'viewer-resize-e2e', version: '1' } })
    @input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized', params: {}))
  end

  def request(method, params)
    @id += 1
    message = { jsonrpc: '2.0', id: @id, method: method, params: params }
    @rpc_log.puts(JSON.generate(message)); @rpc_log.flush
    @input.puts(JSON.generate(message)); @input.flush
    Timeout.timeout(60) do
      loop do
        line = @output.gets or raise 'stdio server closed'
        response = JSON.parse(line)
        @rpc_log.puts(line); @rpc_log.flush
        next unless response['id'] == @id
        raise response['error'].inspect if response['error']
        return response.fetch('result')
      end
    end
  end

  def tool(name, arguments = {}, allow_error: false)
    args = arguments.dup
    args[:session_id] = @session if @session && name != 'session_start'
    result = request('tools/call', { name: name, arguments: args })
    raise "#{name}: #{result.inspect}" if result['isError'] && !allow_error
    result
  end

  def start(width, height)
    result = tool('session_start', { width: width, height: height })
    data = result.fetch('structuredContent')
    @session = data['session_id']
    @workdir = data.fetch('workdir')
    data
  end

  def export(source, destination)
    tool('export_file', { session_path: source, host_path: destination, overwrite: true })
  end

  def close
    begin
      tool('session_stop') if @workdir
    ensure
      @input.close unless @input.closed?
      unless @wait.join(10)
        Process.kill('TERM', @wait.pid)
        @wait.join(5)
      end
      @error_reader.join(5)
      @rpc_log.close
    end
  end
end

BIN = File.expand_path(ENV.fetch('KWIN_MCP_E2E_BIN_DIR'))
SERVER = File.join(BIN, 'kwin-mcp')
PROBE = File.join(BIN, 'viewer-input-probe')
TOUCH = File.join(BIN, 'viewer-touch')

def wait_until(seconds = 30)
  deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + seconds
  loop do
    result = yield
    return result if result
    raise 'condition timed out' if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
    sleep 0.1
  end
end

output = File.expand_path(ARGV.fetch(0))
FileUtils.mkdir_p(output)
outer = nil
inner = nil
touch_holder_pid = nil
begin
  outer = Rpc.new(SERVER, "#{output}/outer-home", "#{output}/outer-server.log",
                  { 'KWIN_MCP_TEST_RESIZABLE_WINDOWS' => '1' })
  data = outer.start(1600, 1000)
  File.write("#{output}/outer-state.json", JSON.pretty_generate(data))
  touch_ready = "#{output}/touch-holder.ready"
  holder_log = File.open("#{output}/touch-holder.log", 'w')
  touch_holder_pid = Process.spawn(TOUCH, '--hold', "#{outer.workdir}/wayland-0", '120', touch_ready,
                                  out: holder_log, err: holder_log)
  holder_log.close
  wait_until(5) { true if File.file?(touch_ready) }
  # Both servers retain the real host systemd manager for bounded scopes.
  # The source server's viewer_open uses the outer isolated Wayland socket.
  inner = Rpc.new(SERVER, "#{output}/inner-home", "#{output}/inner-server.log",
                  { 'WAYLAND_DISPLAY' => "#{outer.workdir}/wayland-0", 'KWIN_MCP_TEST_TOUCH_LOG' => '1' })
  source_data = inner.start(800, 600)
  inner.tool('launch_app', { command: "#{PROBE.shellescape} #{(inner.home + '/events.log').shellescape}" })
  opened = inner.tool('viewer_open')
  viewer_state = opened.fetch('structuredContent').fetch('viewer').fetch('state')
  raise "viewer_open: #{opened.inspect}" unless %w[ready starting].include?(viewer_state)
  File.write("#{output}/inner-state.json", JSON.pretty_generate(source_data.merge('opened' => opened)))
  windows = outer.tool('window_list').fetch('structuredContent').fetch('windows')
  viewer = windows.find { |w| w['resourceClass'].include?('kwin-viewer') || w['title'].start_with?('kwin-viewer') }
  raise "viewer missing: #{windows.inspect}" unless viewer
  id = viewer.fetch('id')
  outer.tool('window_activate', { window_id: id })
  initial = outer.tool('window_list').fetch('structuredContent').fetch('windows').find { |w| w['id'] == id }
  raise 'viewer disappeared before resize' unless initial
  raise "viewer did not open at the session ratio: #{initial.inspect}" unless (initial['w'] * 600 - initial['h'] * 800).abs <= 800
  before = outer.tool('screenshot')
  File.write("#{output}/screenshot-before.json", JSON.pretty_generate(before))
  FileUtils.cp(before.fetch('structuredContent').fetch('path'), "#{output}/screenshot-before.png")
  snapshots = []
  [{ edge: 'bottom', delta: -100 }, { edge: 'horizontal', delta: 100 }].each_with_index do |requested, index|
    previous = outer.tool('window_list').fetch('structuredContent').fetch('windows').find { |w| w['id'] == id }
    raise 'viewer disappeared before interactive resize' unless previous
    if requested[:edge] == 'bottom'
      from_x = previous.fetch('x') + previous.fetch('w') / 2
      from_y = previous.fetch('y') + previous.fetch('h') - 20
      to_x = from_x
      to_y = from_y + requested[:delta]
    else
      delta = requested[:delta]
      if previous.fetch('x') + previous.fetch('w') + delta < 1600
        from_x = previous.fetch('x') + previous.fetch('w') - 20
      else
        from_x = previous.fetch('x') + 20
        delta = -delta
      end
      from_y = previous.fetch('y') + previous.fetch('h') / 2
      to_x = from_x + delta
      to_y = from_y
    end
    resize = "#{TOUCH.shellescape} --resize #{(outer.workdir + '/wayland-0').shellescape} #{from_x} #{from_y} #{to_x} #{to_y}"
    outer.tool('launch_app', { command: resize }, allow_error: true)
    changed = wait_until do
      current = outer.tool('window_list').fetch('structuredContent').fetch('windows').find { |w| w['id'] == id }
      current if current && (current['w'] != previous['w'] || current['h'] != previous['h'])
    end
    sleep 0.5
    geometry = outer.tool('window_list').fetch('structuredContent').fetch('windows').find { |w| w['id'] == id }
    raise 'viewer disappeared after resize' unless geometry
    raise 'compositor did not perform the interactive resize' if geometry['w'] == previous['w'] && geometry['h'] == previous['h']
    snapshots << { requested: requested.merge('dragged_by' => changed.slice('x', 'y', 'w', 'h')), actual: geometry }
    outer.tool('window_activate', { window_id: id })
    shot = outer.tool('screenshot')
    File.write("#{output}/screenshot-#{index}.json", JSON.pretty_generate(shot))
    capture = shot.fetch('structuredContent')
    screenshot_name = index.zero? ? 'screenshot-after-vertical' : 'screenshot-after-horizontal'
    image = "#{output}/#{screenshot_name}.png"
    FileUtils.cp(capture.fetch('path'), image)
    image_width = capture.fetch('width')
    image_height = capture.fetch('height')
    corners = [[2, 2], [image_width - 3, 2], [2, image_height - 3], [image_width - 3, image_height - 3]]
    colors = corners.map do |cx, cy|
      pixel, error, status = Open3.capture3('magick', image, '-crop', "1x1+#{cx}+#{cy}", '-depth', '8', 'rgb:-')
      raise "read screenshot pixel: #{error}" unless status.success?
      pixel.bytes
    end
    expected_colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]]
    raise "picture does not fill viewer corners: #{colors.inspect}" unless colors == expected_colors
    snapshots.last[:corners] = colors
    # The tool's screenshot and mouse coordinates describe the client area.
    x = (geometry.fetch('w') * 0.25).round
    y = (geometry.fetch('h') * 0.75).round
    outer.tool('mouse_click', { x: x, y: y })
    # Send a touch sequence through KWin's fake-input Wayland protocol to this
    # isolated compositor. The viewer forwards it to the inner pointer.
    tx = geometry.fetch('x') + geometry.fetch('w') * 0.75
    ty = geometry.fetch('y') + geometry.fetch('h') * 0.25
    outer.tool('launch_app', { command: "#{TOUCH.shellescape} #{(outer.workdir + '/wayland-0').shellescape} #{tx} #{ty}" }, allow_error: true)
  end
  File.write("#{output}/geometry.json", JSON.pretty_generate({ before: initial, resizes: snapshots }))
  observed_events = wait_until(5) do
    inner.export("#{inner.home}/events.log", "#{output}/events.log")
    lines = File.readlines("#{output}/events.log")
    lines if lines.length >= 4
  end
  FileUtils.cp("#{inner.workdir}/viewer.log", "#{output}/viewer.log")
  if File.exist?("#{inner.workdir}/viewer-pipewire.json")
    FileUtils.cp("#{inner.workdir}/viewer-pipewire.json", "#{output}/viewer-pipewire.json")
  end
  events = observed_events.map { |line| line.split.map(&:to_f) }
  raise "expected mouse and touch after each resize: #{events.inspect}" unless events.length == 4
  events.each_with_index do |event, index|
    expected = index.even? ? [200, 450] : [600, 150]
    raise "input mismatch: #{event.inspect}, expected #{expected.inspect}" unless event[0, 2].zip(expected).all? { |actual, target| (actual - target).abs <= 2 }
    raise "probe is not session-sized: #{event.inspect}" unless event[2, 2] == [800.0, 600.0]
  end
  log = File.read("#{output}/viewer.log")
  failures = snapshots.reject { |s| (s[:actual]['w'] * 600 - s[:actual]['h'] * 800).abs <= 800 }
  if failures.empty?
    puts 'PASS: viewer ratio, mouse and touch mapping after vertical and horizontal drags'
  else
    failures.each do |failure|
      width = failure[:actual]['w'].to_i
      height = failure[:actual]['h'].to_i
      refused = /aspect resize configured=#{width}x#{height} requested=\d+x\d+ applied=Some\(PhysicalSize \{ width: #{width}, height: #{height} \}\)/
      raise "ratio failed without a measured rejected size request: #{failure.inspect}\n#{log}" unless log.match?(refused)
    end
    puts "COMPOSITOR LIMITATION: #{failures.to_json}"
    puts 'PASS: mouse and touch mapping at the compositor-configured geometry'
  end
ensure
  if touch_holder_pid
    begin
      Process.kill('TERM', touch_holder_pid)
    rescue Errno::ESRCH
    end
    begin
      Process.wait(touch_holder_pid)
    rescue Errno::ECHILD
    end
  end
  if inner&.workdir
    %w[viewer.log viewer-pipewire.json].each do |file|
      source = "#{inner.workdir}/#{file}"
      FileUtils.cp(source, "#{output}/#{file}") if File.exist?(source)
    end
  end
  begin
    inner&.close
  ensure
    outer&.close
  end
end
