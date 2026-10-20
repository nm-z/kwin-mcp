#!/usr/bin/env ruby
# Exercise match provenance through the production MCP server on a KDE host.
# Usage: ruby tests/ui_match_sources_e2e.rb SERVER EVIDENCE_DIR
require 'fileutils'
require 'json'
require 'open3'
require 'shellwords'
require 'timeout'

binary, evidence = ARGV
abort 'usage: ui_match_sources_e2e.rb SERVER EVIDENCE_DIR' unless binary && evidence
binary = File.realpath(binary)
evidence = File.expand_path(evidence)
private_home = File.join(evidence, 'home')
%w[.config .cache .local/share .local/state .kde].each do |directory|
  FileUtils.mkdir_p(File.join(private_home, directory))
end
environment = {
  'HOME' => private_home,
  'XDG_CONFIG_HOME' => File.join(private_home, '.config'),
  'XDG_DATA_HOME' => File.join(private_home, '.local/share'),
  'XDG_CACHE_HOME' => File.join(private_home, '.cache'),
  'XDG_STATE_HOME' => File.join(private_home, '.local/state'),
  'KDEHOME' => File.join(private_home, '.kde'),
  'XDG_RUNTIME_DIR' => "/run/user/#{Process.uid}",
  'DBUS_SESSION_BUS_ADDRESS' => "unix:path=/run/user/#{Process.uid}/bus",
  'DISPLAY' => nil, 'WAYLAND_DISPLAY' => nil, 'WAYLAND_SOCKET' => nil,
  'KWIN_MCP_REPO' => File.join(evidence, 'no-source'), 'CARGO' => '/usr/bin/false',
}

def rpc(method, params)
  @sequence += 1
  @input.puts(JSON.generate(jsonrpc: '2.0', id: @sequence, method: method, params: params))
  @input.flush
  response = Timeout.timeout(90) do
    loop do
      line = @output.gets or raise 'unexpected server EOF'
      message = JSON.parse(line)
      next unless message['id'] == @sequence
      break message
    end
  end
  @trace.puts(JSON.generate(method: method, params: params, response: response))
  @trace.flush
  raise "MCP error: #{response}" if response['error'] || response.dig('result', 'isError')
  response.fetch('result')
end

def call(name, arguments = {})
  arguments = arguments.merge(session_id: @session) if @session
  rpc('tools/call', name: name, arguments: arguments)
end

def data(name, arguments = {})
  call(name, arguments).fetch('structuredContent')
end

def capture(tag)
  metadata = data('screenshot')
  FileUtils.cp(metadata.fetch('path'), File.join(@evidence, "#{tag}.png"))
  File.write(File.join(@evidence, "#{tag}.json"), JSON.pretty_generate(metadata))
  metadata
end

def matches(query, tag)
  result = data('find_ui_elements', query: query)
  File.write(File.join(@evidence, "#{tag}.json"), JSON.pretty_generate(result))
  elements = result.fetch('elements')
  raise 'match count disagrees with structured elements' unless result.fetch('matches') == elements.length
  raise 'a stable fixture changed during lookup' if result.fetch('window_changed')
  raise 'stable active-window observations disagree' unless result.fetch('active_window') == result.fetch('active_window_after')
  raise 'lookup did not finish' if result.fetch('incomplete')
  raise 'missing coordinate guidance' unless result.fetch('guidance').include?('page_viewport_css')
  result
end

@evidence = evidence
File.open(File.join(evidence, 'rpc.jsonl'), 'w') do |trace|
  @trace = trace
  File.open(File.join(evidence, 'server.stderr'), 'w') do |diagnostics|
    Open3.popen3(environment, binary, '--autoclean', '--no-viewer') do |input, output, errors, process|
      @input, @output, @sequence = input, output, 0
      reader = Thread.new { IO.copy_stream(errors, diagnostics) }
      begin
        rpc('initialize', protocolVersion: '2025-06-18', capabilities: {}, clientInfo: {name: 'ui-match-sources-e2e', version: '1'})
        input.puts(JSON.generate(jsonrpc: '2.0', method: 'notifications/initialized'))
        input.flush
        @session = data('session_start', width: 1100, height: 800, window_mode: 'native').fetch('session_id')
        titles = ["lookup-left-533-#{Process.pid}", "lookup-right-533-#{Process.pid}"]
        titles.each do |title|
          data('launch_app', command: ['kdialog', '--title', title, '--inputbox', 'Synthetic lookup fixture', title].shelljoin)
        end
        windows = data('window_list').fetch('windows')
        owned = titles.map { |title| windows.find { |window| window.fetch('title').include?(title) } }
        raise "missing native fixture windows: #{windows}" if owned.any?(&:nil?)
        shot = capture('native-before')
        result = matches('OK', 'native-matches')
        raise 'lookup and screenshot name different active windows' unless result.dig('active_window', 'id') == shot.dig('window', 'id')
        native = result.fetch('elements')
        raise 'native lookup did not return both surfaces' unless titles.all? { |title| native.any? { |element| element.dig('source', 'surface', 'name').to_s.include?(title) } }
        native.each do |element|
          raise 'unlabelled native coordinates' unless element.fetch('coordinate_space') == 'atspi_screen_reported'
          source = element.fetch('source')
          raise 'native source is not AT-SPI' unless source.fetch('kind') == 'atspi'
          raise 'missing application identity' if source.dig('application', 'node_id').to_s.empty?
          raise 'missing surface identity' if source.dig('surface', 'node_id').to_s.empty?
        end

        # Identify the left dialog by source, then compare its reported surface
        # dimensions with the screenshot. Qt on Wayland reports a surface-local
        # origin even when Screen coordinates are requested. This fixture's
        # relative bounds are verified independently by the resulting GUI action.
        data('window_activate', window_id: owned[0].fetch('id'))
        left_shot = capture('native-left-active')
        left_result = matches('OK', 'native-left-matches')
        raise 'new lookup retained the previous active window' unless left_result.dig('active_window', 'id') == owned[0].fetch('id')
        left = left_result.fetch('elements').find do |element|
          element.fetch('text') == 'OK' && element.dig('source', 'surface', 'name').to_s.include?(titles[0])
        end
        raise 'left dialog OK button missing its source' unless left
        x, y, width, height = left.fetch('bounds')
        surface_x, surface_y, surface_w, surface_h = left.dig('source', 'surface', 'bounds')
        raise 'AT-SPI surface dimensions do not match this fixture screenshot' unless surface_w == left_shot.dig('window', 'width').round && surface_h == left_shot.dig('window', 'height').round
        raise 'native origin uncertainty is not explained' unless left_result.fetch('guidance').include?('not a verified global origin')
        data('mouse_click', x: x + width / 2 - surface_x, y: y + height / 2 - surface_y)
        close_deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 3
        remaining = []
        loop do
          remaining = data('window_list').fetch('windows')
          break unless remaining.any? { |window| window['id'] == owned[0]['id'] }
          break if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= close_deadline
          sleep 0.05
        end
        raise 'source-guided click did not close the left dialog' if remaining.any? { |window| window['id'] == owned[0]['id'] }
        raise 'source-guided click closed the other dialog' unless remaining.any? { |window| window['id'] == owned[1]['id'] }
        capture('native-after-click')

        # Two real browser windows expose identical labels at viewport
        # coordinates. They must remain distinguishable by target/page source,
        # and those bounds must not be advertised as mouse coordinates.
        page_titles = ["lookup-page-a-533-#{Process.pid}", "lookup-page-b-533-#{Process.pid}"]
        page_urls = page_titles.map { |title| "data:text/html,<title>#{title}</title><button>Needle533</button>" }
        profile = "/tmp/lookup-533-profile-#{Process.pid}"
        page_urls.each do |url|
          command = ['google-chrome-stable', '--no-first-run', '--no-default-browser-check', "--user-data-dir=#{profile}", '--new-window', url]
          launched = data('launch_app', command: command.shelljoin)
          raise "browser fixture has no CDP connection: #{launched}" unless launched['cdp_port']
        end
        capture('browser-active')
        browser_result = matches('Needle533', 'browser-matches')
        browser = browser_result.fetch('elements')
        selected = browser.select { |element| element.fetch('text') == 'Needle533' }
        raise "expected two source-labelled page controls: #{selected}" unless selected.length == 2
        raise 'browser targets are not distinguishable' unless selected.map { |element| element.dig('source', 'target_id') }.uniq.length == 2
        selected.each do |element|
          raise 'viewport CSS bounds were advertised as mouse pixels' unless element.fetch('coordinate_space') == 'page_viewport_css'
          source = element.fetch('source')
          raise 'browser source is not CDP' unless source.fetch('kind') == 'cdp'
          raise 'page identity missing' unless page_urls.include?(source.fetch('page_url')) && page_titles.include?(source.fetch('page_title'))
          raise 'browser target identity missing' if source.fetch('target_id').empty?
        end
        empty = matches('absent-fixture-control-533', 'no-matches')
        raise 'empty search contains matches' unless empty.fetch('elements').empty?
        raise 'empty search lost its active-window context' unless empty.dig('active_window', 'id')
        File.write(File.join(evidence, 'verification.json'), JSON.pretty_generate({
          status: 'passed', native_surfaces: titles, native_click_closed: owned[0]['id'],
          browser_pages: page_titles, browser_targets: selected.map { |element| element.dig('source', 'target_id') },
          session_id: @session,
        }))
        puts 'PASS: native surface attribution, reported-origin uncertainty and source-guided click, two CDP page identities, and empty-result context'
      ensure
        begin
          call('session_stop') if @session
        rescue StandardError => error
          warn "session cleanup: #{error.message}"
        end
        input.close unless input.closed?
        Timeout.timeout(30) { process.value }
        reader.join
      end
    end
  end
end
