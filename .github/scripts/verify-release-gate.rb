#!/usr/bin/env ruby
# The release gate only gates if every publishing job waits behind it. Checks the
# structure of release.yml: `release-inputs` needs `test`, `windows-check` and
# `ci-green`, and every other job reaches `release-inputs` through `needs`.
require "yaml"

jobs = YAML.load_file(File.join(__dir__, "..", "workflows", "release.yml")).fetch("jobs")
gate = %w[test windows-check ci-green release-inputs]
needs = ->(job) { Array(jobs.fetch(job)["needs"]) }

missing = %w[test windows-check ci-green].reject { |job| needs.("release-inputs").include?(job) }
abort "release-inputs must need: #{missing.join(', ')}" unless missing.empty?

reaches = lambda do |job, seen = []|
  return true if job == "release-inputs"
  return false if seen.include?(job)
  needs.(job).any? { |dep| reaches.(dep, seen + [job]) }
end
loose = (jobs.keys - gate).reject { |job| reaches.(job) }
abort "jobs that do not wait for the release gate: #{loose.join(', ')}" unless loose.empty?
# A condition on a publishing job (`if: always()`, `!cancelled()`) could run it
# after the gate failed, so none may have one.
conditional = (jobs.keys - gate).select { |job| jobs.fetch(job).key?("if") }
abort "publishing jobs with an `if`: #{conditional.join(', ')}" unless conditional.empty?
gate_conditional = gate.select { |job| jobs.fetch(job).key?("if") }
abort "gate jobs with an `if`: #{gate_conditional.join(', ')}" unless gate_conditional.empty?
puts "every publishing job waits behind the release gate (#{(jobs.keys - gate).size} jobs)"
