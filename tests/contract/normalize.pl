#!/usr/bin/env perl
# Masks everything in a recorded output that legitimately changes from one
# run to the next, so two binaries (or two runs of one) can be diffed byte
# for byte. Reads stdin, writes stdout. Everything not listed here is kept
# exactly, including JSON spacing, key order and number formatting, because
# those are the contract.
#
# What is masked, and as what:
#   ids         schedule_/job_/run_ + 19 base-36 chars  -> <run:1>, <schedule:2>, ...
#               numbered by first appearance across the whole scenario (the
#               numbering lives in the --state file), so "the run that
#               run --detach returned is the one cancel canceled" still shows.
#   job names   once-<slug>-<6 chars>                    -> once-<slug>-<RAND>
#   timestamps  2026-10-06T01:02:03.456Z                 -> <TIME>
#               (only the 3-decimal ISO form is masked; any other spelling
#               stays visible and fails the diff)
#   log dates   /YYYY-MM-DD/ inside a path               -> /<DATE>/
#   pids        "pid": 123, "pgid": 123, "(pid 123", "pid=123", "kill -9 123"
#                                                        -> <PID>
#   paths       the scenario's temp root (both its /var and /private/var
#               spellings), then the binary under test   -> <TMP>, <BIN>
#   hostname    the machine's hostname, as a whole JSON string -> "<HOST>"
#               (never bare: a short hostname would mask unrelated text)
#   version     the binary's own version string          -> <VERSION>
#   runtime     "bun": "<x>"                             -> "bun": "<BUN>"
#               "commit": "<hex>" (the build's git commit) -> "<COMMIT>"
#               "platform": "darwin|linux", "arch": "arm64|x64"
#                                                        -> <PLATFORM>, <ARCH>
#   relative    "in 5h", "3m ago" (human output only)    -> in <REL>, <REL> ago
#   durations   "(12ms)" style elapsed figures           -> (<MS>ms)
#
# Environment: CONTRACT_TMP (temp root), CONTRACT_TMP_REAL (its realpath),
# CONTRACT_BIN, CONTRACT_HOST, CONTRACT_VERSION. Argument: the state file.
use strict;
use warnings;

my $state_file = shift @ARGV or die "usage: normalize.pl STATE_FILE\n";
my %ids;
my %counter;
if (open my $fh, '<', $state_file) {
  while (my $line = <$fh>) {
    chomp $line;
    my ($id, $label) = split /\t/, $line;
    next unless defined $label;
    $ids{$id} = $label;
    my ($kind, $n) = $label =~ /^<(\w+):(\d+)>$/;
    $counter{$kind} = $n if defined $n && ($counter{$kind} // 0) < $n;
  }
  close $fh;
}

local $/;
my $text = <STDIN>;
$text = '' unless defined $text;

for my $name (qw(CONTRACT_TMP_REAL CONTRACT_TMP)) {
  my $value = $ENV{$name};
  next unless defined $value && $value ne '';
  $text =~ s/\Q$value\E/<TMP>/g;
}
if (defined $ENV{CONTRACT_BIN} && $ENV{CONTRACT_BIN} ne '') {
  $text =~ s/\Q$ENV{CONTRACT_BIN}\E/<BIN>/g;
}
if (defined $ENV{CONTRACT_HOST} && $ENV{CONTRACT_HOST} ne '') {
  $text =~ s/"\Q$ENV{CONTRACT_HOST}\E"/"<HOST>"/g;
}

$text =~ s{\b((?:schedule|job|run)_[0-9a-z]{19})\b}{
  my $id = $1;
  if (!exists $ids{$id}) {
    my ($kind) = $id =~ /^(\w+?)_/;
    $counter{$kind} = ($counter{$kind} // 0) + 1;
    $ids{$id} = "<$kind:$counter{$kind}>";
  }
  $ids{$id};
}ge;
$text =~ s/\b(once-[a-z0-9-]*?)-[0-9a-z]{6}\b/$1-<RAND>/g;

$text =~ s/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z/<TIME>/g;
$text =~ s{/\d{4}-\d{2}-\d{2}/}{/<DATE>/}g;

$text =~ s/("(?:pid|pgid)": ?)\d+/$1<PID>/g;
$text =~ s/\bpid( |=)\d+/pid$1<PID>/g;
$text =~ s/kill -9 \d+/kill -9 <PID>/g;

if (defined $ENV{CONTRACT_VERSION} && $ENV{CONTRACT_VERSION} ne '') {
  my $v = $ENV{CONTRACT_VERSION};
  $text =~ s/"\Q$v\E"/"<VERSION>"/g;
  $text =~ s/(^|[\s(])\Q$v\E(?=$|[\s),])/$1<VERSION>/mg;
}
$text =~ s/("bun": ?)"[^"]*"/$1"<BUN>"/g;
$text =~ s/("platform": ?)"(?:darwin|linux)"/$1"<PLATFORM>"/g;
$text =~ s/("arch": ?)"(?:arm64|x64)"/$1"<ARCH>"/g;
$text =~ s/("commit": ?)"[0-9a-f]{7,40}"/$1"<COMMIT>"/g;

$text =~ s/\bin \d+[dhms]\b/in <REL>/g;
$text =~ s/\b\d+[dhms] ago\b/<REL> ago/g;
$text =~ s/\(\d+ms\)/(<MS>ms)/g;

print $text;

open my $out, '>', $state_file or die "cannot write $state_file: $!\n";
for my $id (sort { $ids{$a} cmp $ids{$b} } keys %ids) {
  print {$out} "$id\t$ids{$id}\n";
}
close $out;
