- link this directory to ${workspace}/.devcontainer
- create a file .devcontainer.ini in ${workspace} like devcontainer.ini.example; it is an INI-style config parsed by init.sh, not a sourced shell script
- top-level lifecycle scripts live under .devcontainer/scripts/; the conf directory stays in place
- initializeCommand generates .devcontainer.compose.yml in the workspace root before docker compose starts
- .devcontainer/docker-compose.template.yml is the shared template used to render that workspace-specific compose file
- the generated compose file carries the resolved build args and mount sources for that workspace; runtime env is written into .devcontainer.runtime.env
- init.sh enforces environment separation by INI section: [build] for build args and bootstrap knobs, [host] for host mount paths, and [env] for container runtime env
- init.sh also ensures a host-side ccache directory exists; by default it uses ~/.cache/ccache on the host and mounts it into the container as ~/.cache/ccache so compiler cache survives container rebuilds
- for host mount directories, init.sh first checks whether the current user already has read/write/execute access; it only applies ACL fixes when access is missing, so permission repair should normally happen once rather than on every run
- init.sh no longer accepts DISPLAY or proxy variables in .devcontainer.ini; DISPLAY is generated inside the container, http_proxy and https_proxy are picked up directly from the host environment when present, and no_proxy falls back to localhost,127.0.0.1/8,10.0.0.0/8 when the host does not provide one
- this avoids conflicts when multiple projects link to the same shared .devcontainer directory
- do not set a fixed container_name in the template; let docker compose derive a unique name from the compose project to avoid name collisions with existing containers
- edit .devcontainer.ini when you want to change build, host, or runtime settings in the corresponding INI section
- you can override the host-side ccache directory by setting CCACHE_DIR under the [host] section in .devcontainer.ini before the container starts; the in-container path stays fixed at ~/.cache/ccache and is not exported as a runtime env var
- init.sh writes build-time bootstrap values such as INCLUDE plus [env] values into .devcontainer.runtime.env; fixed mount paths such as ~/.cache/huggingface and ~/.cache/ccache are resolved by compose volumes and are not exported as runtime env vars
- when the host shell has http_proxy or https_proxy set, init.sh forwards them into both docker build args and the container runtime automatically; no_proxy is also forwarded when set, otherwise init.sh injects the default localhost,127.0.0.1/8,10.0.0.0/8 value
- use ./.devcontainer/bin/devcontainer.sh when you want the same init + docker compose + lifecycle flow without relying on VS Code Dev Containers
- use ./.devcontainer/bin/devcontainer.sh attach to auto-start the container if needed, ensure postCreate/postStart have run, then enter the workspace directory using the container's default login shell
- if host .ssh is missing, init.sh falls back to an empty directory under /tmp instead of failing compose startup
- host `~/.gitconfig` is mounted into the container as `~/.gitconfig`; if it is missing, init.sh falls back to an empty file under `/tmp`
- XPU non-root access is handled by adding the host DRM device groups into the container:
	`video` is needed for `/dev/dri/card*`, and `render` is needed for `/dev/dri/renderD*`.
	`init.sh` resolves the host GIDs and writes them into the generated workspace compose file.
- XPU profiling is split into two levels:
	basic tracing/profiling inside the container gets `CAP_PERFMON` by default;
	deeper hardware metrics may still require host-side kernel settings such as
	`echo 0 | sudo tee /proc/sys/dev/xe/observation_paranoid` on xe, and some tools may
	additionally require mounting `/dev/mem` when you explicitly need that workflow.