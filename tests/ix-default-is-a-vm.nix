# Guard: a flake's `ix.default` output must be a NixOS configuration, never a
# fleet result.
#
# `ix apply` with a bare local target picks between two paths by looking for an
# `ix.default` binding in the flake source (`flake_exposes_ix_default` in ix's
# crates/ix/cli/src/commands/up.rs). Where it finds one it takes the single-VM
# path and builds `ix.default.config.system.build.toplevel`; where it does not
# it converges every `nixosConfigurations` entry that sets `ix.networking`.
#
# The detector reads whether a binding EXISTS, never what it is bound to.
# `mkVm`, `mkDev` and `mkFleet` all return the same fleet result, and a fleet
# result has no `config` -- its attributes are `nodes`, `planValue`,
# `nixosConfigurations`, the lifecycle wrappers. So a flake binding one of those
# to `ix.default` takes the single-VM path and dies with
#
#   flake '...' does not provide attribute 'packages.x86_64-linux.ix.default.config',
#   'legacyPackages.x86_64-linux.ix.default.config' or 'ix.default.config'
#
# naming an attribute path the user never typed. Every in-tree flake that bound
# `ix.default` bound a fleet result, all 15 of them, and no bare `ix apply` in
# any of those directories could work. The right binding is the one `ix init`
# scaffolds: `(mkFleet { ... }).nixosConfigurations.<name>`, which does have
# `config`.
#
# This calls each flake's real `outputs` function rather than matching its text,
# so a binding computed behind a `let` or a helper is classified by what it
# actually is.
{
  lib,
  nixpkgs,
  pkgs,
  ix,
  paths,
}: let
  hostSystem = pkgs.stdenv.hostPlatform.system;

  # The `index` flake input as a project flake sees it: `importIxWasm` plus the
  # builders. The `*For` swap matches `exampleFleetsFor` so any wrapper
  # derivation would target this system rather than the default one.
  indexShim = {
    lib =
      ix
      // {
        mkFleet = ix.mkFleetFor hostSystem;
        mkVm = ix.mkVmFor hostSystem;
        mkDev = ix.mkDevFor hostSystem;
      };
  };

  # Flake inputs this gate can stand in for. Anything else required makes the
  # flake unevaluable here, and it falls to the source check below.
  suppliable = [
    "self"
    "index"
    "nixpkgs"
  ];

  # A directory the walk descends into. `_`- and `.`-prefixed names are skipped
  # with their subtree, matching `discoverTree` in lib/discovery.nix.
  walkable = entries: name:
    entries.${name} == "directory" && !(lib.hasPrefix "_" name) && !(lib.hasPrefix "." name);

  childOf = rel: name:
    if rel == ""
    then name
    else "${rel}/${name}";

  # The repo root is `paths.root` itself, not `paths.root + "/"`.
  dirOf = rel:
    if rel == ""
    then paths.root
    else paths.root + "/${rel}";

  fileOf = rel:
    if rel == ""
    then "flake.nix"
    else "${rel}/flake.nix";

  # Every directory in the repo holding a `flake.nix`, the repo root included.
  # Enumerated by walking rather than listed, so a project flake added anywhere
  # is covered on the next eval with no registry edit. Only `readDir` and the
  # 30-odd `flake.nix` files are read (0.4s on the tree this was written
  # against), and nothing derived from `paths.root` reaches the derivation, so
  # this does not couple the check to the whole tree.
  flakeDirs = let
    walk = path: rel: let
      entries = builtins.readDir path;
    in
      lib.optional ((entries."flake.nix" or null) == "regular") rel
      ++ lib.concatMap (name: walk (path + "/${name}") (childOf rel name)) (
        builtins.filter (walkable entries) (builtins.attrNames entries)
      );
  in
    walk paths.root "";

  shapeOf = binding:
    if binding == null
    then "no-binding"
    else if binding ? config
    then "vm"
    else "fleet";

  classify = rel: let
    dir = dirOf rel;
    flake = import (dir + "/flake.nix");
    args = builtins.functionArgs flake.outputs;
    # `functionArgs` marks an argument false when it has no default, so this is
    # exactly the set of inputs that would throw if we called `outputs`.
    unsuppliable =
      builtins.filter (name: !(builtins.elem name suppliable) && !args.${name})
      (builtins.attrNames args);
    outputs = builtins.addErrorContext "while evaluating the flake outputs of ${fileOf rel} for tests/ix-default-is-a-vm.nix" (
      flake.outputs {
        self = dir;
        index = indexShim;
        inherit nixpkgs;
      }
    );
  in {
    file = fileOf rel;
    # Lazy on purpose: `outputs` throws for a flake we cannot supply, so the
    # shape is only asked for once `unsuppliable` says it is safe to ask.
    kind =
      if unsuppliable != []
      then "unevaluable"
      else shapeOf ((outputs.ix or {}).default or null);
  };

  classified = map classify flakeDirs;
  kind = name: builtins.filter (entry: entry.kind == name) classified;
  files = entries: map (entry: entry.file) entries;

  # A flake this gate cannot call gets the source check instead. Weaker, and
  # said out loud rather than skipped: a silent skip would make a new broken
  # binding in such a flake indistinguishable from no binding at all.
  unevaluableMentioningIxDefault =
    builtins.filter (
      entry:
        lib.hasInfix "ix.default"
        (builtins.readFile (paths.root + "/${entry.file}"))
    )
    (kind "unevaluable");

  # The passing state of the main assertion is an ABSENCE: with every in-tree
  # binding removed, `kind "fleet"` is empty whether the check works or the walk
  # silently found nothing. These two keep it honest.
  #
  # The predicate still discriminates. Both shapes come out of one `mkVm` call:
  # the fleet result it returns, and the NixOS configuration inside it, which is
  # the binding `ix init` scaffolds.
  fleetShaped = (ix.mkVmFor hostSystem) {modules = [];};
  vmShaped = fleetShaped.nixosConfigurations.default;

  # And the walk still found flakes to classify. Floors with slack, not exact
  # counts: they exist to fail when the walk returns nothing, not to be
  # maintained as flakes come and go.
  discoveredFloor = 20;
  evaluableFloor = 15;
  evaluable = builtins.length classified - builtins.length (kind "unevaluable");
in
  assert lib.assertMsg (!(fleetShaped ? config) && vmShaped ? config) ''
    tests/ix-default-is-a-vm.nix cannot tell a fleet result from a VM any more:
    `mkVm {}` reports config=${lib.boolToString (fleetShaped ? config)} and its
    `nixosConfigurations.default` reports config=${lib.boolToString (vmShaped ? config)}.

    The first should be false and the second true. Until they are, the
    ix.default check below passes by testing nothing.
  '';
  assert lib.assertMsg (builtins.length classified >= discoveredFloor && evaluable >= evaluableFloor) ''
    tests/ix-default-is-a-vm.nix found ${toString (builtins.length classified)} flake.nix
    files (floor ${toString discoveredFloor}), ${toString evaluable} of them evaluable
    (floor ${toString evaluableFloor}).

    The walk over paths.root returned too little to be checking anything. Either
    the tree moved under it or `flakeDirs` is broken; fix that before lowering
    the floors.
  '';
  assert lib.assertMsg (kind "fleet" == []) ''
    These flakes bind `ix.default` to a fleet result:

      ${lib.concatStringsSep "\n      " (files (kind "fleet"))}

    `index.lib.mkVm`, `mkDev` and `mkFleet` all return a fleet result, which has
    no `config`. `ix apply` with a bare local target prefers a flake's
    `ix.default` and builds `ix.default.config.system.build.toplevel` from it, so
    this binding fails the apply on a missing attribute instead of converging the
    flake's nodes.

    Drop the `ix.default = ...;` line and keep `inherit (vm) nixosConfigurations;`.
    A bare `ix apply` then converges every node the flake declares. If you want
    the single-VM path instead, bind the configuration rather than the fleet:
    `ix.default = vm.nixosConfigurations.<name>;`.
  '';
  assert lib.assertMsg (unevaluableMentioningIxDefault == []) ''
    These flakes mention `ix.default` and take a flake input this gate cannot
    supply, so it could not evaluate the binding to check it:

      ${lib.concatStringsSep "\n      " (files unevaluableMentioningIxDefault)}

    Supply the input from tests/ix-default-is-a-vm.nix (`suppliable`), or check
    by hand that the bound value has `config` and say so here.
  '';
    pkgs.runCommand "ix-default-is-a-vm-guard" {} "touch $out"
