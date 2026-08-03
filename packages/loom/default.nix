{
  lib,
  ix,
  repoPackages,
}: let
  inherit (ix) pkgs;

  version = "0.1.0"; # keep in sync with mix.exs
  elixir = ix.languages.elixir.toolchain pkgs {version = "1.20";};
  erlang = ix.languages.erlang.toolchain pkgs {version = "29";};

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./lib
      ./test
      ./config
      ./mix.exs
      ./mix.lock
      ./.formatter.exs
    ];
  };

  mixFodDeps = pkgs.beamPackages.fetchMixDeps {
    pname = "loom-deps";
    inherit version src elixir;
    mixEnv = "test";
    inherit ((ix.pins.loadPins ./pins.json).mix-deps) hash;
  };

  elixirCheck = ix.buildElixirCheck pkgs {
    pname = "loom-check";
    inherit version src elixir erlang;
    mixDeps = mixFodDeps;
    extraNativeBuildInputs = [pkgs.bash];
    setupHook = ''
      patchShebangs "$sourceRoot/test/support/fake-ix"
    '';
  };

  ixCli = pkgs.callPackage ./ix-cli.nix {inherit ix;};
  claudeCode = repoPackages.claude-code.override {
    systemTools.Agent = false;
  };
  loomPrompt = pkgs.writeText "loom-system-prompt.md" ''
    You are the lead coding agent in a Loom control VM. Work normally and talk
    to the user normally. For every delegated subagent, use the index MCP exec
    tool and call `Agents.spawn(brief, backend: :claude, cwd: cwd)`. Loom runs
    that agent in its own snapshot fork of this VM and stops the fork when the
    agent finishes. Use `Agents.status`, `Agents.await`, `Agents.send`, and
    `Agents.report` to coordinate them. The native Agent tool is intentionally
    unavailable because it would run the child inside this control VM.
  '';
  # Compiled launchers (packages/loom/launch); the wrappers below bake the
  # claude/ix/prompt store paths that the shell versions took via replaceVars.
  launchBin = binary:
    lib.getExe' (ix.cargoUnit.selectBinaryWithTests ix.rustWorkspace.units {
      inherit binary;
      meta = {
        description = "loom launcher binary ${binary}";
        license = lib.licenses.mit;
        mainProgram = binary;
      };
    })
    binary;

  package = pkgs.stdenv.mkDerivation {
    pname = "loom";
    inherit version src;
    strictDeps = true;
    __darwinAllowLocalNetworking = true;

    nativeBuildInputs = [
      erlang
      elixir
      (ix.elixirHex {inherit pkgs elixir;})
      pkgs.makeWrapper
    ];

    env = {
      MIX_ENV = "prod";
      HEX_OFFLINE = "1";
      LANG = "C.UTF-8";
      LC_CTYPE = "C.UTF-8";
    };

    postUnpack = ''
      export MIX_HOME="$TEMPDIR/mix"
      export HEX_HOME="$TEMPDIR/hex"
    '';

    buildPhase = ''
      # shell
      runHook preBuild
      mix release --no-deps-check --path "$out/lib/loom"
      runHook postBuild
    '';

    installPhase = ''
      # shell
      runHook preInstall
      makeWrapper "$out/lib/loom/bin/loom" "$out/bin/loom-console" \
        --set RELEASE_DISTRIBUTION none \
        --set-default RELEASE_TMP /tmp \
        --add-flags start_iex
      makeWrapper ${launchBin "loom-launch"} "$out/bin/loom" \
        --set LOOM_CLAUDE_BIN ${lib.getExe claudeCode} \
        --set LOOM_PROMPT_FILE ${loomPrompt}
      makeWrapper ${launchBin "loom-claude"} "$out/bin/loom-claude" \
        --set LOOM_CLAUDE_BIN ${lib.getExe claudeCode}
      makeWrapper ${launchBin "loom-remote-claude"} "$out/bin/loom-remote-claude" \
        --set LOOM_IX_BIN ${lib.getExe ixCli}
      runHook postInstall
    '';

    meta = {
      description = "Snapshot-forked Claude subagents driven through ix VMs";
      license = lib.licenses.mit;
      mainProgram = "loom";
      platforms = ["x86_64-linux"];
    };
  };
in
  package.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        inherit claudeCode ixCli;
        mcp = repoPackages.mcp-ex;
        tests.elixir = elixirCheck;
      };
  })
