#!/usr/bin/env ruby
# Use private mount namespaces and synthetic storage; never mount over host data.
require 'fileutils'
require 'json'
require 'open3'
require 'rbconfig'
require 'tmpdir'
require_relative 'fixture_paths'

if ARGV.first == '--namespace'
  _, server, root, mode, uid, gid, groups = ARGV
  abort 'namespace fixture requires root' unless Process.euid.zero?
  disk = File.join(root, 'cache space', 'kwin-mcp', 'kwin-mcp-2000000000-1')
  abort 'private bind failed' unless system('mount', '--bind', File.join(root,'outside'), File.join(disk,'mounted'))
  Process.groups = groups.split(',').map { |value| Integer(value) }
  Process::GID.change_privilege(Integer(gid))
  Process::UID.change_privilege(Integer(uid))
  environment = {'TMPDIR'=>File.join(root,'runtime space'), 'XDG_CACHE_HOME'=>File.join(root,'cache space'), 'HOME'=>File.join(root,'home')}
  output, status = Open3.capture2e(environment, server, '--sweep-workdirs', '2000000000')
  observed = {mode:mode, status:status.exitstatus, output:output,
    sentinel:File.exist?(File.join(root,'outside','keep')) ? File.read(File.join(root,'outside','keep')) : nil,
    outside_mode:File.stat(File.join(root,'outside')).mode & 0o777,
    runtime_present:File.directory?(File.join(root,'runtime space','kwin-mcp-2000000000-1')),
    disk_present:File.directory?(disk)}
  puts JSON.generate(observed)
  abort 'sweep crossed a mounted directory or discarded retry ownership' unless observed[:sentinel]=='outside sentinel' && observed[:outside_mode]==0o500 && observed[:runtime_present] && observed[:disk_present]
  exit
end

server, evidence = ARGV
abort 'usage: orphan_mount_guard_e2e.rb SERVER EVIDENCE' unless server && evidence
server = File.realpath(server)
FileUtils.mkdir_p(evidence)
abort 'fixture PID is not outside the kernel PID range' unless Integer(File.read('/proc/sys/kernel/pid_max')) < 2_000_000_000
%w[leased unleased].each do |mode|
  Dir.mktmpdir('km-orphan-mount-', FixturePaths.root) do |root|
    runtime = File.join(root,'runtime space','kwin-mcp-2000000000-1')
    disk = File.join(root,'cache space','kwin-mcp','kwin-mcp-2000000000-1')
    FileUtils.mkdir_p([runtime,File.join(disk,'mounted'),File.join(root,'outside'),File.join(root,'home')])
    File.write(File.join(root,'outside','keep'), 'outside sentinel')
    File.chmod(0o500, File.join(root,'outside'))
    File.write(File.join(runtime,'.kwin-mcp-autoclean'), "kwin-mcp-autoclean-v1 2000000000 2000000000\n") if mode == 'leased'
    command = ['sudo','-n','unshare','--mount','--propagation','private','--',RbConfig.ruby,File.realpath(__FILE__),'--namespace',server,root,mode,Process.uid.to_s,Process.gid.to_s,Process.groups.join(',')]
    output, status = Open3.capture2e(*command)
    File.write(File.join(evidence,"#{mode}.log"),output)
    raise "#{mode}: #{output}" unless status.success?
    # The private namespace is gone; cleanup can now remove only the owned trees.
    env = {'TMPDIR'=>File.join(root,'runtime space'),'XDG_CACHE_HOME'=>File.join(root,'cache space'),'HOME'=>File.join(root,'home')}
    cleanup, status = Open3.capture2e(env,server,'--sweep-workdirs','2000000000')
    raise cleanup unless status.success? && !File.exist?(runtime) && !File.exist?(disk)
    raise 'outside sentinel changed on retry' unless File.read(File.join(root,'outside','keep'))=='outside sentinel'
    puts "PASS #{mode}: mounted data and permissions preserved, cleanup succeeds after unmount"
  ensure
    File.chmod(0o700, File.join(root,'outside')) if File.directory?(File.join(root,'outside'))
  end
end
