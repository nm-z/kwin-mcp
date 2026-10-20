# Shared storage policy for real-system fixtures.
module FixturePaths
  def self.root
    root = ENV.values_at('KWIN_MCP_TEST_TMPDIR', 'TMPDIR').find { |value| value && !value.empty? } || '/var/tmp'
    raise "fixture root must be absolute: #{root}" unless root.start_with?('/')
    raise "fixture root does not exist: #{root}" unless File.directory?(root)
    root
  end
end
