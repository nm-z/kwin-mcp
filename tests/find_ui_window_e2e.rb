#!/usr/bin/env ruby
# Production stdio + real KDialog/Chrome windows, without touching user profiles.
require 'json'
require 'open3'
require 'timeout'
require 'tmpdir'
require 'fileutils'
require 'shellwords'

binary, evidence = ARGV
abort 'usage: find_ui_window_e2e.rb /path/to/kwin-mcp /path/to/evidence' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
FileUtils.mkdir_p(evidence)
baseline = ENV['KWIN_MCP_E2E_BASELINE'] == '1'

class WindowLookupProbe
  attr_reader :session
  attr_writer :lookup_peer
  def initialize(binary, home, evidence, session:nil)
    @home, @evidence, @id = home, evidence, 0
    %w[.config .cache .local/share .local/state .kde].each { |path| FileUtils.mkdir_p(File.join(home, path)) }
    env = {
      'HOME'=>home, 'XDG_CONFIG_HOME'=>"#{home}/.config", 'XDG_CACHE_HOME'=>"#{home}/.cache",
      'XDG_DATA_HOME'=>"#{home}/.local/share", 'XDG_STATE_HOME'=>"#{home}/.local/state",
      'KDEHOME'=>"#{home}/.kde", 'XDG_RUNTIME_DIR'=>"/run/user/#{Process.uid}",
      'DBUS_SESSION_BUS_ADDRESS'=>"unix:path=/run/user/#{Process.uid}/bus",
      'DISPLAY'=>nil, 'WAYLAND_DISPLAY'=>nil, 'WAYLAND_SOCKET'=>nil,
      'KWIN_MCP_REPO'=>"#{home}/no-source", 'CARGO'=>'/usr/bin/false'
    }
    @trace = File.open(File.join(evidence, 'rpc.jsonl'), 'w')
    @errors = File.open(File.join(evidence, 'server.stderr'), 'w')
    @input, @output, @process = Open3.popen2(env, binary, '--autoclean', err:@errors)
    rpc('initialize', protocolVersion:'2024-11-05', capabilities:{}, clientInfo:{name:'find-ui-window-proof',version:'1'})
    @input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized')); @input.flush
    @owns_session = session.nil?
    if session
      @session = session
    else
      start = call('session_start', width:1000, height:700, browser_profiles:'fresh', window_mode:'native')
      @session = start.fetch('structuredContent').fetch('session_id')
    end
  end
  def rpc(method, params)
    @id += 1
    @input.puts(JSON.generate(jsonrpc:'2.0',id:@id,method:method,params:params)); @input.flush
    value = Timeout.timeout(90) do
      loop do
        value = JSON.parse(@output.gets || (raise 'server EOF'))
        break value if value['id'] == @id
      end
    end
    @trace.puts(JSON.generate(method:method,params:params,response:value)); @trace.flush
    raise value.inspect if value['error'] || value.dig('result','isError')
    value.fetch('result')
  end
  def call(name, args={})
    args = args.merge(session_id:@session) if @session
    rpc('tools/call',name:name,arguments:args)
  end
  def data(name,args={}); call(name,args).fetch('structuredContent'); end
  def screen(tag)
    result = data('screenshot')
    FileUtils.cp(result.fetch('path'),File.join(@evidence,"#{tag}.png"))
    result
  end
  def active
    data('window_list').fetch('windows').find { |window| window.fetch('active') } || (raise 'no active window')
  end
  def lookup(query, baseline:false)
    expected = active
    tree = call('accessibility_tree', max_depth:3, show_elements:true)
    File.write(File.join(@evidence,'window-frames.json'),JSON.pretty_generate(tree))
    result = (@lookup_peer || self).data('find_ui_elements',query:query)
    File.write(File.join(@evidence,"lookup-#{query.gsub(/\W/,'-')}.json"),JSON.pretty_generate(result))
    if baseline
      raise 'baseline did not expose multiple-window matches' unless result.fetch('matches') >= 2
      raise 'baseline unexpectedly binds its matches to a window' if result.key?('window')
      puts JSON.generate(baseline:true,query:query,matches_from_multiple_windows:result.fetch('matches'),window_identity_missing:true)
      return result
    end
    raise 'incomplete lookup' if result.fetch('incomplete')
    raise 'wrong coordinate frame' unless result.fetch('coordinate_frame') == 'window_client_pixels'
    raise 'lookup names another window' unless result.fetch('window').fetch('id') == expected.fetch('id')
    raise 'missing accessibility identity' if result.fetch('accessibility_frame').to_s.empty?
    raise 'unexpectedly claims occlusion was checked' unless result.fetch('occlusion') == 'not_checked'
    elements = result.fetch('elements')
    raise 'lookup mixed multiple windows or lost the label/field pair' unless elements.length == 2
    raise 'mixed window identities' unless elements.all? { |element| element.fetch('window_id') == expected.fetch('id') }
    [expected,elements]
  end
  def click_field(elements)
    entry = elements.find { |element| element.fetch('states').include?('focused') } ||
            elements.find { |element| %w[entry text].include?(element.fetch('role')) } ||
            (raise "editable field not found: #{elements.inspect}")
    data('mouse_click',x:(entry.fetch('x')+entry.fetch('width')/2.0).round,
                       y:(entry.fetch('y')+entry.fetch('height')/2.0).round)
    entry
  end
  def kdialog(baseline)
    %w[one two].each do |name|
      command = "kdialog --title 'Lookup #{name} 533' --inputbox 'Common field 533' > #{Shellwords.escape(File.join(@home,"#{name}.txt"))}"
      data('launch_app',command:command)
    end
    if baseline
      lookup('Common field 533',baseline:true)
      screen('baseline-two-windows')
      return
    end
    expected,elements = lookup('Common field 533')
    raise 'native fixture did not exercise a nonzero origin' if expected.fetch('x') == 0 && expected.fetch('y') == 0
    raise 'wrong native window is active' unless expected.fetch('title').include?('two 533')
    entry = click_field(elements)
    data('keyboard_type',text:'second533')
    screen('native-field-after-click')
    data('keyboard_key',key:'Return')
    exported = File.join(@evidence,'second-field.txt')
    data('export_file',session_path:File.join(@home,'two.txt'),host_path:exported)
    raise 'window-relative click typed into the wrong native field' unless File.read(exported) == "second533\n"
    # The second dialog closed; the still-open first dialog must now match alone.
    first = data('window_list').fetch('windows').find { |window| window.fetch('title').include?('one 533') } || (raise 'first dialog was unexpectedly closed')
    data('window_activate',window_id:first.fetch('id'))
    _,first_elements = lookup('Common field 533')
    click_field(first_elements)
    data('keyboard_type',text:'first533')
    data('keyboard_key',key:'Return')
    exported_first = File.join(@evidence,'first-field.txt')
    data('export_file',session_path:File.join(@home,'one.txt'),host_path:exported_first)
    raise 'first field value differs' unless File.read(exported_first) == "first533\n"
    puts JSON.generate(case:'native',window_origin:expected.values_at('x','y'),relative_entry:entry.slice('x','y','width','height'),independent_exported_values:true)
  end
  def chrome
    %w[one two].each do |name|
      path = File.join(@home,"browser-#{name}.html")
      html = "<!doctype html><title>Browser #{name} 533</title><label for='field'>Browser field 533</label><input id='field' aria-label='Browser field 533' autofocus>"
      File.write(path,html)
      # The HOME twin is created before these fixtures; export the fixture into
      # the owned session through an ordinary application command.
      command = "bash -c #{Shellwords.escape("printf %s #{Shellwords.escape(html)} > #{Shellwords.escape(path)}; exec google-chrome-stable --no-first-run --no-default-browser-check --new-window --force-renderer-accessibility --window-size=640,480 #{Shellwords.escape("file://#{path}")}")}"
      data('launch_app',command:command)
    end
    # Chrome finishes its page load asynchronously after creating the window.
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC)+20
    until active.fetch('title').include?('Browser two 533')
      raise 'Chrome fixture did not finish loading' if Process.clock_gettime(Process::CLOCK_MONOTONIC)>=deadline
      sleep 0.1
    end
    expected,elements = lookup('Browser field 533')
    click_field(elements)
    data('keyboard_type',text:'browser533')
    screen('browser-field-after-click')
    data('keyboard_key',key:'ctrl+a')
    data('keyboard_key',key:'ctrl+c')
    copied = data('clipboard_read').fetch('text')
    File.write(File.join(@evidence,'browser-copied.txt'),copied)
    raise 'browser field click selected another surface' unless copied == 'browser533'
    puts JSON.generate(case:'chrome',window_id:expected.fetch('id'),title:expected.fetch('title'),independent_clipboard_value:true)
  end
  def close
    call('session_stop') if @session && @owns_session
  ensure
    @input&.close
    if @process
      begin
        Timeout.timeout(30) { @process.value }
      rescue Timeout::Error
        Process.kill('TERM',@process.pid) rescue Errno::ESRCH
        Timeout.timeout(30) { @process.value }
      end
    end
    @output&.close; @errors&.close; @trace&.close
  end
end

Dir.mktmpdir('k533-',ENV.fetch('KWIN_MCP_TEST_TMPDIR','/var/tmp')) do |home|
  client = nil
  gateway = nil
  begin
    client = WindowLookupProbe.new(binary,home,evidence)
    unless baseline
      gateway_evidence = File.join(evidence,'gateway')
      FileUtils.mkdir_p(gateway_evidence)
      gateway = WindowLookupProbe.new(binary,home,gateway_evidence,session:client.session)
      client.lookup_peer = gateway
    end
    client.kdialog(baseline)
    client.chrome unless baseline
    puts JSON.generate(forwarded_lookup:!baseline,owner_and_gateway_separate_processes:!baseline)
  ensure
    gateway&.close
    client&.close
  end
end
