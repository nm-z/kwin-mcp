#!/usr/bin/env ruby
# Fit aggregate timing statistics from the first 4 MiB of the published Aalto ZIP.
# Usage: curl --range 0-4194303 --max-filesize 4194304 URL -o aalto-prefix.zip
#        ruby scripts/fit-input-profile.rb aalto-prefix.zip > data/input-profile.json
require 'json'
require 'zlib'
bytes = File.binread(ARGV.fetch(0))
offset = 0
participants = 0
fast = 0
samples = Hash.new { |h,k| h[k] = [] }
while bytes.byteslice(offset, 4) == "PK\x03\x04".b
  header = bytes.byteslice(offset + 4, 26).unpack('v5V3v2')
  size, method = header[6], header[2]
  start = offset + 30 + header[8] + header[9]
  break if start + size > bytes.bytesize
  name = bytes.byteslice(offset + 30, header[8])
  if name.end_with?('_keystrokes.txt')
    raise 'unsupported ZIP method' unless method == 8
    inflater = Zlib::Inflate.new(-15)
    rows = inflater.inflate(bytes.byteslice(start, size)).lines.drop(1).map { |line| line.chomp.split("\t", -1) }
    inflater.close
    participants += 1
    pairs = rows.each_cons(2).filter_map do |a,b|
      next unless a.length == 9 && b.length == 9 && a[1] == b[1]
      next unless a[7].length == 1 && b[7].length == 1 && (a[7]+b[7]).ascii_only?
      hold = a[6].to_i - a[5].to_i
      interval = b[5].to_i - a[5].to_i
      next unless hold.between?(10,500) && interval.between?(10,2000)
      [a[7].downcase+b[7].downcase, hold.to_f, interval.to_f]
    end
    mean = pairs.sum { |_,_,iki| iki } / [pairs.length,1].max
    # Paper's fast cohort is above approximately 78 WPM. Use the timing analogue
    # 12000 / mean IKI, with five characters per word; normalize to 100 WPM.
    if pairs.length >= 100 && mean.between?(60,12000.0/78)
      fast += 1
      scale = 120.0 / mean
      pairs.each do |pair,hold,iki|
        value = [Math.log(hold*scale), Math.log(iki*scale)]
        samples[pair] << value
        samples['*'] << value
      end
    end
  end
  offset = start + size
end
stats = samples.sort.filter_map do |pair, values|
  next if values.length < 20
  n = values.length
  mh = values.sum(&:first)/n
  mi = values.sum(&:last)/n
  vh = values.sum { |h,_| (h-mh)**2 }/n
  vi = values.sum { |_,i| (i-mi)**2 }/n
  cov = values.sum { |h,i| (h-mh)*(i-mi) }/n
  [pair, [n,mh,Math.sqrt(vh),mi,Math.sqrt(vi),cov/Math.sqrt(vh*vi)].map { |v| v.is_a?(Float) ? v.round(8) : v }]
end.to_h
puts JSON.pretty_generate({
  source: 'https://userinterfaces.aalto.fi/136Mkeystrokes/data/Keystrokes.zip',
  citation: 'Dhakal et al., CHI 2018, doi:10.1145/3173574.3174220',
  sample: 'First 4194304 archive bytes; complete participant entries only',
  participants: participants, fast_participants: fast,
  pair_samples: samples['*'].length, base_wpm: 100,
  fit: 'Bivariate lognormal hold/press-to-press timing; per-participant mean IKI normalized to 120 ms; lowercase ASCII bigrams with at least 20 samples',
  filters: 'Adjacent printable rows within a sentence; hold 10..500 ms, IKI 10..2000 ms; >=100 pairs per participant; mean IKI 60..153.846 ms',
  fields: %w[count log_hold_mean log_hold_sd log_iki_mean log_iki_sd correlation],
  bigrams: stats
})
