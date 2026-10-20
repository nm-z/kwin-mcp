#!/usr/bin/env ruby
# Run the already-built production stdio server against an owned Konsole.
# Usage: ruby tests/terminal_receipts_e2e.rb BUILT_SERVER EVIDENCE_DIRECTORY [COMMAND_CHARACTERS]
require 'json'
require 'open3'
require 'timeout'
require 'fileutils'
require 'securerandom'

class TerminalReceiptProbe
  def initialize(binary, evidence, command_characters = 129)
    @evidence = File.expand_path(evidence)
    @home = File.join(@evidence,'home')
    %w[.config .cache .local/share .local/state .kde].each { |dir| FileUtils.mkdir_p(File.join(@home,dir)) }
    @nonce = SecureRandom.hex(8)
    @payload = "receipt-#{@nonce}: AaZz 0123456789 !? [] {} () +=_-/.:,;@#%"
    @session_file = File.join(@home,"terminal-proof-#{@nonce}.txt")
    # Two intentional trials; append exposes duplicated or missing execution.
    @command = "printf '%s\\n' '#{@payload}' >> \"$HOME/terminal-proof-#{@nonce}.txt\""
    padding = Integer(command_characters) - @command.length
    raise 'command length must be at least 129 characters' if padding.negative?
    @payload += 'x' * padding
    @command = "printf '%s\\n' '#{@payload}' >> \"$HOME/terminal-proof-#{@nonce}.txt\""
    @trace = File.open(File.join(@evidence,'rpc.jsonl'),'w')
    @input_count = Hash.new(0)
    environment = {
      'HOME'=>@home, 'XDG_CONFIG_HOME'=>File.join(@home,'.config'),
      'XDG_DATA_HOME'=>File.join(@home,'.local/share'), 'XDG_CACHE_HOME'=>File.join(@home,'.cache'),
      'XDG_STATE_HOME'=>File.join(@home,'.local/state'), 'KDEHOME'=>File.join(@home,'.kde'),
      'XDG_RUNTIME_DIR'=>ENV['XDG_RUNTIME_DIR'],
      'DBUS_SESSION_BUS_ADDRESS'=>ENV['DBUS_SESSION_BUS_ADDRESS'],
      'KWIN_MCP_REPO'=>File.join(@evidence,'no-source'), 'CARGO'=>'/usr/bin/false',
      'DISPLAY'=>nil, 'WAYLAND_DISPLAY'=>nil, 'WAYLAND_SOCKET'=>nil,
    }
    @input,@output,errors,@process = Open3.popen3(environment,File.realpath(binary),'--autoclean','--width','1232','--height','924')
    @stderr = Thread.new do
      File.open(File.join(@evidence,'server.stderr'),'w') { |file| IO.copy_stream(errors,file) }
    end
    @id = 0
    success(rpc('initialize',protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'terminal-receipts-e2e',version:'1'}))
    @input.puts(JSON.generate(jsonrpc:'2.0',method:'notifications/initialized')); @input.flush
    started = raw_call('session_start',browser_profiles:'fresh',startup_timeout_seconds:60)
    @session_id = started.dig('error','data','session_id')
    @session_id = success(started).fetch('structuredContent').fetch('session_id')
  rescue StandardError
    stop
    raise
  end

  def clock
    Process.clock_gettime(Process::CLOCK_MONOTONIC)
  end

  def rpc(method,params)
    @id += 1
    began = clock
    @input.puts(JSON.generate(jsonrpc:'2.0',id:@id,method:method,params:params)); @input.flush
    response = Timeout.timeout(150) do
      loop do
        line = @output.gets or raise 'unexpected server EOF'
        message = JSON.parse(line)
        break message if message['id'] == @id
      end
    end
    @trace.puts(JSON.generate(method:method,params:params,elapsed_seconds:clock-began,response:response)); @trace.flush
    response
  end

  def raw_call(name,arguments={})
    @input_count[name] += 1 if %w[keyboard_type keyboard_key clipboard_write].include?(name)
    arguments = arguments.merge(session_id:@session_id) if @session_id
    rpc('tools/call',name:name,arguments:arguments)
  end

  def success(response)
    raise "MCP failure: #{response}" if response['error'] || response.dig('result','isError')
    response.fetch('result')
  end

  def call(name,arguments={})
    success(raw_call(name,arguments)).fetch('structuredContent')
  end

  def screen(tag)
    data = call('screenshot')
    png = File.binread(data.fetch('path'))
    raise 'invalid PNG capture' unless png.start_with?("\x89PNG\r\n\x1a\n".b)
    raise 'PNG dimensions disagree' unless png[16,8].unpack('NN') == data.values_at('width','height')
    File.binwrite(File.join(@evidence,"#{tag}.png"),png)
    File.write(File.join(@evidence,"#{tag}.json"),JSON.pretty_generate(data))
    data
  end

  def exported_lines(tag,count)
    expected = (@payload+"\n")*count
    destination = File.join(@evidence,"#{tag}.txt")
    deadline = clock+10
    loop do
      response = raw_call('export_file',session_path:@session_file,host_path:destination,overwrite:true)
      if !response['error'] && !response.dig('result','isError') && File.file?(destination)
        observed = File.binread(destination)
        return observed if observed == expected
        if observed.bytesize >= expected.bytesize
          raise "terminal output differs (missing/duplicate/changed input): #{observed.inspect}"
        end
      end
      raise "terminal command did not produce exact bytes within 10s: #{response}" if clock >= deadline
      sleep 0.1
    end
  end

  def wait_for_terminal(window_id)
    deadline = clock + 15
    loop do
      windows = call('window_list').fetch('windows')
      response = raw_call('accessibility_tree', app_name: 'konsole', role: 'terminal', show_elements: true)
      success(response)
      tree = response.fetch('result').fetch('content').filter_map { |item| item['text'] if item['type'] == 'text' }.join("\n")
      File.write(File.join(@evidence, 'terminal-focus.tree.txt'), tree)
      File.write(File.join(@evidence, 'terminal-focus.windows.json'), JSON.pretty_generate(windows))
      focused = tree.lines.any? do |line|
        role, _name, states = line.strip.split("\t", -1)
        role == 'terminal' && %w[focused visible].all? { |state| states.to_s.split('|').include?(state) }
      end
      active = windows.any? { |window| window['id'] == window_id && window['active'] && !window['dialog'] }
      return if active && focused
      raise 'Konsole has no focused terminal; no text sent (see terminal-focus artifacts)' if clock >= deadline
      sleep 0.2
    end
  end

  def run
    call('launch_app',command:'konsole --separate --nofork -e bash --noprofile --norc')
    windows = call('window_list').fetch('windows')
    terminal = windows.find { |window| window['active'] && (window.fetch('resourceClass','').downcase.include?('konsole') || window.fetch('title','').downcase.include?('konsole')) }
    raise "owned Konsole not active: #{windows}" unless terminal
    call('window_activate',window_id:terminal.fetch('id'))
    wait_for_terminal(terminal.fetch('id'))
    screen('terminal-before-input')
    File.write(File.join(@evidence,'expected-command.txt'),@command)
    File.write(File.join(@evidence,'expected-payload.txt'),@payload+"\n")

    began = clock
    typed_response = raw_call('keyboard_type',text:@command)
    typing_seconds = clock-began
    File.write(File.join(@evidence,'typing-receipt.json'),JSON.pretty_generate(typed_response))
    screen('typed-before-return')
    call('keyboard_key',key:'Return')
    typed_bytes = exported_lines('typed-output',1)
    typed = success(typed_response).fetch('structuredContent')
    raise "typing omitted complete submission: #{typed}" unless typed['submission_complete'] == true
    raise "typing suggested an unsafe replay: #{typed}" unless typed['retry_safe'] == false
    raise 'typing character count differs' unless typed.fetch('submitted_characters') == @command.length
    delivery = typed.fetch('delivery')
    raise "terminal receipt falsely verifies editable text: #{delivery}" unless delivery['status'] == 'unverified'
    raise 'compositor acknowledgement missing' unless delivery['compositor_acknowledged'] == true
    raise 'terminal receipt invented field character counts' unless delivery['expected_field_characters'].nil? && delivery['observed_field_characters'].nil?
    screen('typed-after-return')

    began = clock
    stored = call('clipboard_write',target:'session',text:@command)
    paste_key = call('keyboard_key',key:'ctrl+shift+v')
    paste_seconds = clock-began
    raise 'clipboard store targets another surface' unless stored.fetch('target') == 'session'
    raise 'clipboard store changed text length' unless stored.fetch('length') == @command.length
    raise 'clipboard storage falsely claims application delivery' if stored.key?('delivery') || stored.key?('submission_complete')
    raise 'paste shortcut receipt missing' unless paste_key.fetch('action') == 'key' && paste_key.fetch('key') == 'ctrl+shift+v'
    raise 'key receipt falsely claims verified application effect' if paste_key['application_effect'] == 'verified'
    clipboard = call('clipboard_read')
    raise 'session clipboard differs' unless clipboard.fetch('text') == @command
    screen('pasted-before-return')
    call('keyboard_key',key:'Return')
    pasted_bytes = exported_lines('pasted-output',2)
    screen('pasted-after-return')
    final_window = call('window_list').fetch('windows').find { |window| window['active'] }
    raise 'input moved to another window' unless final_window && final_window['id'] == terminal['id']
    raise 'input was replayed' unless @input_count['keyboard_type'] == 1 && @input_count['clipboard_write'] == 1 && @input_count['keyboard_key'] == 3
    raise "clipboard path was not faster in this run: typed=#{typing_seconds}, pasted=#{paste_seconds}" unless paste_seconds < typing_seconds
    proof = {
      session_id:@session_id,window_id:terminal['id'],command_characters:@command.length,
      typing_seconds:typing_seconds,session_clipboard_paste_seconds:paste_seconds,
      speedup:typing_seconds/paste_seconds,typing_delivery:delivery,
      typing_retry_safe:typed['retry_safe'],clipboard_receipt:stored,paste_key_receipt:paste_key,
      independent_typed_bytes:typed_bytes.bytesize,independent_pasted_bytes:pasted_bytes.bytesize,
      exact_single_execution_per_trial:true,keyboard_type_calls:@input_count['keyboard_type'],
      session_clipboard_writes:@input_count['clipboard_write'],key_calls:@input_count['keyboard_key'],
    }
    File.write(File.join(@evidence,'verification.json'),JSON.pretty_generate(proof))
    puts JSON.generate(proof)
  end

  def stop
    call('session_stop') if @session_id
  ensure
    @session_id = nil
    @input&.close
    Timeout.timeout(30) { @process.value } if @process
    @stderr&.join
    @trace&.close
  end
end

binary,evidence,command_characters = ARGV
abort 'usage: terminal_receipts_e2e.rb BUILT_SERVER EVIDENCE_DIRECTORY [COMMAND_CHARACTERS]' unless binary && evidence
probe = nil
begin
  probe = TerminalReceiptProbe.new(binary,evidence,command_characters || 129)
  probe.run
ensure
  probe&.stop
end
