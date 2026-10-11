#!/usr/bin/env ruby
# Run with private, already-built binaries on the KDE host. Compilation belongs
# on a build host. The outer stdio session provides an isolated KWin desktop for
# the viewer; a second production stdio session supplies the picture and input.
require 'json'
require 'open3'
require 'fileutils'
require 'timeout'
require 'shellwords'

class Rpc
  attr_reader :home, :session, :workdir

  def initialize(binary, home, log)
    @home = home
    %w[.config .local/share .cache .local/state .kde].each { |p| FileUtils.mkdir_p(File.join(home, p)) }
    env = { 'HOME' => home, 'XDG_CONFIG_HOME' => "#{home}/.config", 'XDG_DATA_HOME' => "#{home}/.local/share",
            'XDG_CACHE_HOME' => "#{home}/.cache", 'XDG_STATE_HOME' => "#{home}/.local/state", 'KDEHOME' => "#{home}/.kde",
            'KWIN_MCP_REPO' => "#{home}/no-repository", 'KDE_SESSION_VERSION' => '6' }
    @input, @output, error, @wait = Open3.popen3(env, binary, '--autoclean')
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

if ARGV.first == '--inner'
  control = ARGV.fetch(1)
  FileUtils.mkdir_p(control)
  inner = nil
  begin
    inner = Rpc.new(SERVER, "#{control}/inner-home", "#{control}/inner-server.log")
    data = inner.start(800, 600)
    inner.tool('launch_app', { command: "#{PROBE.shellescape} #{(inner.home + '/events.log').shellescape}" })
    opened = inner.tool('viewer_open')
    File.write("#{control}/ready.json", JSON.pretty_generate(data.merge('opened' => opened)))
    wait_until(600) { File.exist?("#{control}/stop") }
    inner.export("#{inner.home}/events.log", "#{control}/events.log")
    FileUtils.cp("#{inner.workdir}/viewer.log", "#{control}/viewer.log")
    FileUtils.cp("#{inner.workdir}/viewer-pipewire.json", "#{control}/viewer-pipewire.json") if File.exist?("#{inner.workdir}/viewer-pipewire.json")
  rescue StandardError => error
    File.write("#{control}/error.txt", "#{error.full_message}\n")
    raise
  ensure
    inner&.close
    File.write("#{control}/finished", 'stopped')
  end
  exit
end

output = File.expand_path(ARGV.fetch(0))
FileUtils.mkdir_p(output)
outer = nil
inner_ready = false
inner_stopped = false
begin
  outer = Rpc.new(SERVER, "#{output}/outer-home", "#{output}/outer-server.log")
  data = outer.start(1600, 1000)
  control = "#{outer.home}/control"
  upper = "#{outer.home}/.cache/kwin-mcp/#{File.basename(outer.workdir)}/overlay-upper"
  inner_state = "#{upper}/control/ready.json"
  script = File.expand_path(__FILE__)
  command = "KWIN_MCP_E2E_BIN_DIR=#{BIN.shellescape} ruby #{script.shellescape} --inner #{control.shellescape}"
  outer.tool('launch_app', { command: command }, allow_error: true)
  wait_until(60) do
    error = "#{upper}/control/error.txt"
    raise File.read(error) if File.exist?(error)
    File.exist?(inner_state)
  end
  inner_ready = true
  FileUtils.cp(inner_state, "#{output}/inner-state.json")
  windows = outer.tool('window_list').fetch('structuredContent').fetch('windows')
  viewer = windows.find { |w| w['resourceClass'].include?('kwin-viewer') || w['title'].start_with?('kwin-viewer') }
  raise "viewer missing: #{windows.inspect}" unless viewer
  id = viewer.fetch('id')
  outer.tool('window_activate', { window_id: id })
  # This KWin belongs exclusively to the outer test session.
  outer.tool('launch_app', { command: "kdotool windowstate --remove MAXIMIZED #{id.shellescape} windowsize #{id.shellescape} 800 600 windowmove #{id.shellescape} 100 100" }, allow_error: true)
  snapshots = []
  [ [900, 600], ['bottom-edge', 100] ].each_with_index do |requested, index|
    if index.zero?
      width, height = requested
      outer.tool('launch_app', { command: "kdotool windowsize #{id.shellescape} #{width} #{height}" }, allow_error: true)
    else
      previous = snapshots.last[:actual]
      from_x = previous.fetch('x') + previous.fetch('w') / 2
      from_y = previous.fetch('y') + previous.fetch('h') - 20
      to_y = from_y + requested.last
      outer.tool('launch_app', { command: "#{TOUCH.shellescape} --resize #{(outer.workdir + '/wayland-0').shellescape} #{from_x} #{from_y} #{from_x} #{to_y}" }, allow_error: true)
    end
    sleep 0.5
    geometry = outer.tool('window_list').fetch('structuredContent').fetch('windows').find { |w| w['id'] == id }
    raise 'viewer disappeared' unless geometry
    snapshots << { requested: requested, actual: geometry }
    if index.positive?
      previous = snapshots[index - 1][:actual]
      raise 'compositor did not perform the interactive resize' if geometry['w'] == previous['w'] && geometry['h'] == previous['h']
    end
    outer.tool('window_activate', { window_id: id })
    shot = outer.tool('screenshot')
    File.write("#{output}/screenshot-#{index}.json", JSON.pretty_generate(shot))
    capture = shot.fetch('structuredContent')
    image = "#{output}/screenshot-#{index}.png"
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
    x = geometry.fetch('w') * 0.25
    y = geometry.fetch('h') * 0.75
    outer.tool('mouse_click', { x: x, y: y })
    # Send a real wl_touch sequence to this isolated KWin, at the same
    # window-relative point. The viewer forwards it to the inner pointer.
    tx = geometry.fetch('x') + geometry.fetch('w') * 0.75
    ty = geometry.fetch('y') + geometry.fetch('h') * 0.25
    outer.tool('launch_app', { command: "#{TOUCH.shellescape} #{(outer.workdir + '/wayland-0').shellescape} #{tx} #{ty}" }, allow_error: true)
  end
  File.write("#{output}/geometry.json", JSON.pretty_generate(snapshots))
  outer.tool('launch_app', { command: "touch #{(control + '/stop').shellescape}" }, allow_error: true)
  wait_until(30) { File.exist?("#{upper}/control/finished") }
  inner_stopped = true
  %w[events.log viewer.log inner-server.log inner-server.log.rpc error.txt viewer-pipewire.json].each do |file|
    source = "#{upper}/control/#{file}"
    FileUtils.cp(source, "#{output}/#{file}") if File.exist?(source)
  end
  raise File.read("#{output}/error.txt") if File.exist?("#{output}/error.txt")
  events = File.readlines("#{output}/events.log").map { |line| line.split.map(&:to_f) }
  raise "expected mouse and touch after each resize: #{events.inspect}" unless events.length == 4
  events.each_with_index do |event, index|
    expected = index.even? ? [200, 450] : [600, 150]
    raise "input mismatch: #{event.inspect}, expected #{expected.inspect}" unless event[0, 2].zip(expected).all? { |actual, target| (actual - target).abs <= 2 }
    raise "probe is not session-sized: #{event.inspect}" unless event[2, 2] == [800.0, 600.0]
  end
  log = File.read("#{output}/viewer.log")
  failures = snapshots.reject { |s| (s[:actual]['w'] * 600 - s[:actual]['h'] * 800).abs <= 800 }
  if failures.empty?
    puts 'PASS: viewer ratio, mouse and touch mapping after horizontal and vertical changes'
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
  if outer && inner_ready && !inner_stopped
    begin
      outer.tool('launch_app', { command: "touch #{(control + '/stop').shellescape}" }, allow_error: true)
      wait_until(30) { File.exist?("#{upper}/control/finished") }
    rescue StandardError => error
      File.write("#{output}/cleanup-error.txt", error.full_message)
    end
    %w[events.log viewer.log inner-server.log inner-server.log.rpc error.txt].each do |file|
      source = "#{upper}/control/#{file}"
      FileUtils.cp(source, "#{output}/#{file}") if File.exist?(source)
    end
  end
  outer&.close
end
