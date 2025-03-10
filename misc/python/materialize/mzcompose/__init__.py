# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

"""The implementation of the mzcompose system for Docker compositions.

For an overview of what mzcompose is and why it exists, see the [user-facing
documentation][user-docs].

[user-docs]: https://github.com/MaterializeInc/materialize/blob/main/doc/developer/mzbuild.md
"""

import argparse
import copy
import importlib
import importlib.abc
import importlib.util
import inspect
import os
import subprocess
import sys
import time
import traceback
from contextlib import contextmanager
from dataclasses import dataclass
from inspect import getmembers, isfunction
from ssl import SSLContext
from tempfile import TemporaryFile
from typing import (
    Any,
    Callable,
    Dict,
    Iterable,
    Iterator,
    List,
    Literal,
    Optional,
    OrderedDict,
    Sequence,
    Tuple,
    TypedDict,
    TypeVar,
    Union,
    cast,
)

import pg8000
import sqlparse
import yaml
from pg8000 import Connection, Cursor

from materialize import mzbuild, spawn, ui
from materialize.mzcompose import loader
from materialize.ui import UIError

T = TypeVar("T")
say = ui.speaker("C> ")


class UnknownCompositionError(UIError):
    """The specified composition was unknown."""

    def __init__(self, name: str):
        super().__init__(f"unknown composition {name!r}")


class Composition:
    """A loaded mzcompose.py file."""

    @dataclass
    class TestResult:
        duration: float
        error: Optional[str]

    def __init__(
        self,
        repo: mzbuild.Repository,
        name: str,
        preserve_ports: bool = False,
        silent: bool = False,
        munge_services: bool = True,
        project_name: Optional[str] = None,
    ):
        self.name = name
        self.description = None
        self.repo = repo
        self.preserve_ports = preserve_ports
        self.project_name = project_name
        self.silent = silent
        self.workflows: Dict[str, Callable[..., None]] = {}
        self.test_results: OrderedDict[str, Composition.TestResult] = OrderedDict()

        if name in self.repo.compositions:
            self.path = self.repo.compositions[name]
        else:
            raise UnknownCompositionError(name)

        self.compose: dict[str, Any] = {
            "version": "3.7",
            "services": {},
        }

        # Load the mzcompose.py file, if one exists
        mzcompose_py = self.path / "mzcompose.py"
        if mzcompose_py.exists():
            spec = importlib.util.spec_from_file_location("mzcompose", mzcompose_py)
            assert spec
            module = importlib.util.module_from_spec(spec)
            assert isinstance(spec.loader, importlib.abc.Loader)
            loader.composition_path = self.path
            spec.loader.exec_module(module)
            loader.composition_path = None
            self.description = inspect.getdoc(module)
            for name, fn in getmembers(module, isfunction):
                if name.startswith("workflow_"):
                    # The name of the workflow is the name of the function
                    # with the "workflow_" prefix stripped and any underscores
                    # replaced with dashes.
                    name = name[len("workflow_") :].replace("_", "-")
                    self.workflows[name] = fn

            for python_service in getattr(module, "SERVICES", []):
                name = python_service.name
                if name in self.compose["services"]:
                    raise UIError(f"service {name!r} specified more than once")
                self.compose["services"][name] = python_service.config

        # Add default volumes
        self.compose.setdefault("volumes", {}).update(
            {
                "mzdata": None,
                "pgdata": None,
                "mydata": None,
                "tmp": None,
                "secrets": None,
            }
        )

        # The CLI driver will handle acquiring these dependencies.
        if munge_services:
            self.dependencies = self._munge_services(self.compose["services"].items())

        # Emit the munged configuration to a temporary file so that we can later
        # pass it to Docker Compose.
        self.file = TemporaryFile(mode="w")
        os.set_inheritable(self.file.fileno(), True)
        self._write_compose()

    def _munge_services(
        self, services: List[Tuple[str, dict]]
    ) -> mzbuild.DependencySet:
        images = []

        for name, config in services:
            # Remember any mzbuild references.
            if "mzbuild" in config:
                image_name = config["mzbuild"]
                if image_name not in self.repo.images:
                    raise UIError(f"mzcompose: unknown image {image_name}")
                image = self.repo.images[image_name]
                images.append(image)

            if "propagate_uid_gid" in config:
                if config["propagate_uid_gid"]:
                    config["user"] = f"{os.getuid()}:{os.getgid()}"
                del config["propagate_uid_gid"]

            ports = config.setdefault("ports", [])
            for i, port in enumerate(ports):
                if self.preserve_ports and not ":" in str(port):
                    # If preserving ports, bind the container port to the same
                    # host port, assuming the host port is available.
                    ports[i] = f"{port}:{port}"
                elif ":" in str(port) and not config.get("allow_host_ports", False):
                    # Raise an error for host-bound ports, unless
                    # `allow_host_ports` is `True`
                    raise UIError(
                        "programming error: disallowed host port in service {name!r}",
                        hint='Add `"allow_host_ports": True` to the service config to disable this check.',
                    )

            if "allow_host_ports" in config:
                config.pop("allow_host_ports")

            if self.repo.rd.coverage:
                coverage_volume = "./coverage:/coverage"
                if coverage_volume not in config.get("volumes", []):
                    # Emit coverage information to a file in a directory that is
                    # bind-mounted to the "coverage" directory on the host. We
                    # inject the configuration to all services for simplicity, but
                    # this only have an effect if the service runs instrumented Rust
                    # binaries.
                    config.setdefault("volumes", []).append(coverage_volume)

                llvm_profile_file = (
                    f"LLVM_PROFILE_FILE=/coverage/{name}-%p-%9m%c.profraw"
                )
                for i, env in enumerate(config.get("environment", [])):
                    # Make sure we don't have duplicate environment entries.
                    if env.startswith("LLVM_PROFILE_FILE="):
                        config["environment"][i] = llvm_profile_file
                        break
                else:
                    config.setdefault("environment", []).append(llvm_profile_file)

        # Determine mzbuild specs and inject them into services accordingly.
        deps = self.repo.resolve_dependencies(images)
        for _name, config in services:
            if "mzbuild" in config:
                config["image"] = deps[config["mzbuild"]].spec()
                del config["mzbuild"]

        return deps

    def _write_compose(self) -> None:
        self.file.seek(0)
        self.file.truncate()
        yaml.dump(self.compose, self.file)
        self.file.flush()

    def invoke(
        self,
        *args: str,
        capture: bool = False,
        capture_stderr: bool = False,
        stdin: Optional[str] = None,
        check: bool = True,
    ) -> subprocess.CompletedProcess:
        """Invoke `docker compose` on the rendered composition.

        Args:
            args: The arguments to pass to `docker compose`.
            capture: Whether to capture the child's stdout stream.
            capture_stderr: Whether to capture the child's stderr stream.
            input: A string to provide as stdin for the command.
        """

        if not self.silent:
            print(f"$ docker compose {' '.join(args)}", file=sys.stderr)

        self.file.seek(0)

        stdout = None
        if capture:
            stdout = subprocess.PIPE
        stderr = None
        if capture_stderr:
            stderr = subprocess.PIPE
        project_name_args = (
            ("--project-name", self.project_name) if self.project_name else ()
        )

        try:
            return subprocess.run(
                [
                    "docker",
                    "compose",
                    f"-f/dev/fd/{self.file.fileno()}",
                    "--project-directory",
                    self.path,
                    *project_name_args,
                    *args,
                ],
                close_fds=False,
                check=check,
                stdout=stdout,
                stderr=stderr,
                input=stdin,
                text=True,
                bufsize=1,
            )
        except subprocess.CalledProcessError as e:
            if e.stdout:
                print(e.stdout)
            raise UIError(f"running docker compose failed (exit status {e.returncode})")

    def port(self, service: str, private_port: Union[int, str]) -> int:
        """Get the public port for a service's private port.

        Delegates to `docker compose port`. See that command's help for details.

        Args:
            service: The name of a service in the composition.
            private_port: A private port exposed by the service.
        """
        proc = self.invoke("port", service, str(private_port), capture=True)
        if not proc.stdout.strip():
            raise UIError(
                f"service f{service!r} is not exposing port {private_port!r}",
                hint="is the service running?",
            )
        return int(proc.stdout.split(":")[1])

    def default_port(self, service: str) -> int:
        """Get the default public port for a service.

        Args:
            service: The name of a service in the composition.
        """
        ports = self.compose["services"][service]["ports"]
        if not ports:
            raise UIError(f"service f{service!r} does not expose any ports")
        private_port = str(ports[0]).split(":")[-1]
        return self.port(service, private_port)

    def workflow(self, name: str, *args: str) -> None:
        """Run a workflow in the composition.

        Raises a `KeyError` if the workflow does not exist.

        Args:
            name: The name of the workflow to run.
            args: The arguments to pass to the workflow function.
        """
        ui.header(f"Running workflow {name}")
        func = self.workflows[name]
        parser = WorkflowArgumentParser(name, inspect.getdoc(func), list(args))
        try:
            loader.composition_path = self.path
            if len(inspect.signature(func).parameters) > 1:
                func(self, parser)
            else:
                # If the workflow doesn't have an `args` parameter, parse them here
                # with an empty parser to reject bogus arguments and to handle the
                # trivial help message.
                parser.parse_args()
                func(self)
        finally:
            loader.composition_path = None

    @contextmanager
    def override(self, *services: "Service") -> Iterator[None]:
        """Temporarily update the composition with the specified services.

        The services must already exist in the composition. They restored to
        their old definitions when the `with` block ends. Note that the service
        definition is written in its entirety; i.e., the configuration is not
        deep merged but replaced wholesale.

        Lest you are tempted to change this function to allow dynamically
        injecting new services: do not do this! These services will not be
        visible to other commands, like `mzcompose run`, `mzcompose logs`, or
        `mzcompose down`, which makes debugging or inspecting the composition
        challenging.
        """
        # Remember the old composition.
        old_compose = copy.deepcopy(self.compose)

        # Update the composition with the new service definitions.
        deps = self._munge_services([(s.name, cast(dict, s.config)) for s in services])
        for service in services:
            self.compose["services"][service.name] = service.config

        # Re-acquire dependencies, as the override may have swapped an `image`
        # config for an `mzbuild` config.
        deps.acquire()

        self._write_compose()

        # Ensure image freshness
        self.pull_if_variable([service.name for service in services])

        try:
            # Run the next composition.
            yield
        finally:
            # Restore the old composition.
            self.compose = old_compose
            self._write_compose()

    @contextmanager
    def test_case(self, name: str) -> Iterator[None]:
        """Execute a test case.

        This context manager provides a very lightweight testing framework. If
        the body of the context manager raises an exception, the test case is
        considered to have failed; otherwise it is considered to have succeeded.
        In either case the execution time and status of the test are recorded in
        `test_results`.

        Example:
            A simple workflow that executes a table-driven test:

            ```
            @dataclass
            class TestCase:
                name: str
                files: list[str]

            test_cases = [
                TestCase(name="short", files=["quicktests.td"]),
                TestCase(name="long", files=["longtest1.td", "longtest2.td"]),
            ]

            def workflow_default(c: Composition):
                for tc in test_cases:
                    with c.test_case(tc.name):
                        c.run("testdrive", *tc.files)
            ```

        Args:
            name: The name of the test case. Must be unique across the lifetime
                of a composition.
        """
        if name in self.test_results:
            raise UIError(f"test case {name} executed twice")
        ui.header(f"Running test case {name}")
        error = None
        start_time = time.time()
        try:
            yield
            ui.header(f"mzcompose: test case {name} succeeded")
        except Exception as e:
            error = str(e)
            if isinstance(e, UIError):
                print(f"mzcompose: test case {name} failed: {e}", file=sys.stderr)
            else:
                print(f"mzcompose: test case {name} failed:", file=sys.stderr)
                traceback.print_exc()
        elapsed = time.time() - start_time
        self.test_results[name] = Composition.TestResult(elapsed, error)

    def sql_connection(
        self,
        service: str = "materialized",
        user: str = "materialize",
        port: Optional[int] = None,
        password: Optional[str] = None,
    ) -> Connection:
        """Get a connection (with autocommit enabled) to the materialized service."""
        port = self.port(service, port) if port else self.default_port(service)
        conn = pg8000.connect(host="localhost", user=user, password=password, port=port)
        conn.autocommit = True
        return conn

    def sql_cursor(
        self,
        service: str = "materialized",
        user: str = "materialize",
        port: Optional[int] = None,
        password: Optional[str] = None,
    ) -> Cursor:
        """Get a cursor to run SQL queries against the materialized service."""
        conn = self.sql_connection(service, user, port, password)
        return conn.cursor()

    def sql(
        self,
        sql: str,
        service: str = "materialized",
        user: str = "materialize",
        port: Optional[int] = None,
        password: Optional[str] = None,
        print_statement: bool = True,
    ) -> None:
        """Run a batch of SQL statements against the materialized service."""
        with self.sql_cursor(
            service=service, user=user, port=port, password=password
        ) as cursor:
            for statement in sqlparse.split(sql):
                if print_statement:
                    print(f"> {statement}")
                cursor.execute(statement)

    def sql_query(
        self,
        sql: str,
        service: str = "materialized",
        user: str = "materialize",
        password: Optional[str] = None,
    ) -> Any:
        """Execute and return results of a SQL query."""
        with self.sql_cursor(service=service, user=user, password=password) as cursor:
            cursor.execute(sql)
            return cursor.fetchall()

    def run(
        self,
        service: str,
        *args: str,
        detach: bool = False,
        rm: bool = False,
        env_extra: Dict[str, str] = {},
        capture: bool = False,
        capture_stderr: bool = False,
        stdin: Optional[str] = None,
        entrypoint: Optional[str] = None,
        check: bool = True,
    ) -> subprocess.CompletedProcess:
        """Run a one-off command in a service.

        Delegates to `docker compose run`. See that command's help for details.
        Note that unlike `docker compose run`, any services whose definitions
        have changed are rebuilt (like `docker compose up` would do) before the
        command is executed.

        Args:
            service: The name of a service in the composition.
            args: Arguments to pass to the service's entrypoint.
            detach: Run the container in the background.
            stdin: read STDIN from a string.
            env_extra: Additional environment variables to set in the container.
            rm: Remove container after run.
            capture: Capture the stdout of the `docker compose` invocation.
            capture_stderr: Capture the stderr of the `docker compose` invocation.
        """
        # Restart any dependencies whose definitions have changed. The trick,
        # taken from Buildkite's Docker Compose plugin, is to run an `up`
        # command that requests zero instances of the requested service.
        self.invoke("up", "--detach", "--scale", f"{service}=0", service)
        return self.invoke(
            "run",
            *(["--entrypoint", entrypoint] if entrypoint else []),
            *(f"-e{k}={v}" for k, v in env_extra.items()),
            *(["--detach"] if detach else []),
            *(["--rm"] if rm else []),
            service,
            *args,
            capture=capture,
            capture_stderr=capture_stderr,
            stdin=stdin,
            check=check,
        )

    def exec(
        self,
        service: str,
        *args: str,
        detach: bool = False,
        capture: bool = False,
        capture_stderr: bool = False,
        stdin: Optional[str] = None,
        check: bool = True,
    ) -> subprocess.CompletedProcess:
        """Execute a one-off command in a service's running container

        Delegates to `docker compose exec`.

        Args:
            service: The service whose container will be used.
            command: The command to run.
            args: Arguments to pass to the command.
            detach: Run the container in the background.
            stdin: read STDIN from a string.
        """

        return self.invoke(
            "exec",
            *(["--detach"] if detach else []),
            "-T",
            service,
            *(
                self.compose["services"][service]["entrypoint"]
                if "entrypoint" in self.compose["services"][service]
                else []
            ),
            *args,
            capture=capture,
            capture_stderr=capture_stderr,
            stdin=stdin,
            check=check,
        )

    def pull_if_variable(self, services: List[str]) -> None:
        """Pull fresh service images in case the tag indicates thee underlying image may change over time.

        Args:
            services: List of service names
        """

        for service in services:
            if "image" in self.compose["services"][service] and any(
                self.compose["services"][service]["image"].endswith(tag)
                for tag in [":latest", ":unstable", ":rolling"]
            ):
                self.invoke("pull", service)

    def up(
        self,
        *services: str,
        detach: bool = True,
        wait: bool = True,
        persistent: bool = False,
    ) -> None:
        """Build, (re)create, and start the named services.

        Delegates to `docker compose up`. See that command's help for details.

        Args:
            services: The names of services in the composition.
            detach: Run containers in the background.
            wait: Wait for health checks to complete before returning.
                Implies `detach` mode.
            persistent: Replace the container's entrypoint and command with
                `sleep infinity` so that additional commands can be scheduled
                on the container with `Composition.exec`.
        """
        if persistent:
            old_compose = copy.deepcopy(self.compose)
            for service in self.compose["services"].values():
                service["entrypoint"] = ["sleep", "infinity"]
                service["command"] = []
            self._write_compose()

        self.invoke(
            "up",
            *(["--detach"] if detach else []),
            *(["--wait"] if wait else []),
            *services,
        )

        if persistent:
            self.compose = old_compose
            self._write_compose()

    def down(self, destroy_volumes: bool = True, remove_orphans: bool = True) -> None:
        """Stop and remove resources.

        Delegates to `docker compose down`. See that command's help for details.

        Args:
            destroy_volumes: Remove named volumes and anonymous volumes attached
                to containers.
        """
        self.invoke(
            "down",
            *(["--volumes"] if destroy_volumes else []),
            *(["--remove-orphans"] if remove_orphans else []),
        )

    def stop(self, *services: str) -> None:
        """Stop the docker containers for the named services.

        Delegates to `docker compose stop`. See that command's help for details.

        Args:
            services: The names of services in the composition.
        """
        self.invoke("stop", *services)

    def kill(self, *services: str, signal: str = "SIGKILL") -> None:
        """Force stop service containers.

        Delegates to `docker compose kill`. See that command's help for details.

        Args:
            services: The names of services in the composition.
            signal: The signal to deliver.
        """
        self.invoke("kill", f"-s{signal}", *services)

    def pause(self, *services: str) -> None:
        """Pause service containers.

        Delegates to `docker compose pause`. See that command's help for details.

        Args:
            services: The names of services in the composition.
        """
        self.invoke("pause", *services)

    def unpause(self, *services: str) -> None:
        """Unpause service containers

        Delegates to `docker compose unpause`. See that command's help for details.

        Args:
            services: The names of services in the composition.
        """
        self.invoke("unpause", *services)

    def rm(
        self, *services: str, stop: bool = True, destroy_volumes: bool = True
    ) -> None:
        """Remove stopped service containers.

        Delegates to `docker compose rm`. See that command's help for details.

        Args:
            services: The names of services in the composition.
            stop: Stop the containers if necessary.
            destroy_volumes: Destroy any anonymous volumes associated with the
                service. Note that this does not destroy any named volumes
                attached to the service.
        """
        self.invoke(
            "rm",
            "--force",
            *(["--stop"] if stop else []),
            *(["-v"] if destroy_volumes else []),
            *services,
        )

    def rm_volumes(self, *volumes: str, force: bool = False) -> None:
        """Remove the named volumes.

        Args:
            volumes: The names of volumes in the composition.
            force: Whether to force the removal (i.e., don't error if the
                volume does not exist).
        """
        volumes = (f"{self.name}_{v}" for v in volumes)
        spawn.runv(
            ["docker", "volume", "rm", *(["--force"] if force else []), *volumes]
        )

    def sleep(self, duration: float) -> None:
        """Sleep for the specified duration in seconds."""
        print(f"Sleeping for {duration} seconds...")
        time.sleep(duration)

    def testdrive(
        self,
        input: str,
        service: str = "testdrive",
        persistent: bool = True,
        args: List[str] = [],
    ) -> None:
        """Run a string as a testdrive script.

        Args:
            args: Additional arguments to pass to testdrive
            service: Optional name of the testdrive service to use.
            input: The string to execute.
            persistent: Whether a persistent testdrive container will be used.
        """

        if persistent:
            self.exec(service, *args, stdin=input)
        else:
            self.run(service, *args, stdin=input)


class ServiceHealthcheck(TypedDict, total=False):
    """Configuration for a check to determine whether the containers for this
    service are healthy."""

    test: Union[List[str], str]
    """A specification of a command to run."""

    interval: str
    """The interval at which to run the healthcheck."""

    timeout: str
    """The maximum amount of time that the test command can run before it
    is considered failed."""

    retries: int
    """The number of consecutive healthchecks that must fail for the container
    to be considered unhealthy."""

    start_period: str
    """The period after container start during which failing healthchecks will
    not be counted towards the retry limit."""


class ServiceDependency(TypedDict, total=False):
    """Configuration for a check to determine whether the containers for this
    service are healthy."""

    condition: str
    """Condition under which a dependency is considered satisfied."""


class ServiceConfig(TypedDict, total=False):
    """The definition of a service in Docker Compose.

    This object corresponds directly to the YAML definition in a
    docker-compose.yml file, plus two mzcompose-specific attributes. Full
    details are available in [Services top-level element][ref] chapter of the
    Compose Specification.

    [ref]: https://github.com/compose-spec/compose-spec/blob/master/spec.md#services-top-level-element
    """

    mzbuild: str
    """The name of an mzbuild image to dynamically acquire before invoking
    Docker Compose.

    This is a mzcompose-extension to Docker Compose. The image must exist in
    the repository. If `mzbuild` is set, neither `build` nor `image` should be
    set.
    """

    propagate_uid_gid: bool
    """Request that the Docker image be run with the user ID and group ID of the
    host user.

    This is an mzcompose extension to Docker Compose. It is equivalent to
    passing `--user $(id -u):$(id -g)` to `docker run`. The defualt is `False`.
    """

    allow_host_ports: bool
    """Allow the service to map host ports in its `ports` configuration.

    This option is intended only for compositions that are meant to be run as
    background services in developer environments. Compositions that are
    isolated tests of Materialize should *not* enable this option, as it leads
    to unnecessary conflicts between compositions. Compositions that publish the
    same host port cannot be run concurrently. Instead, users should use the
    `mzcompose port` command to discover the ephemeral host port mapped to the
    desired container port, or to use `mzcompose up --preserve-ports`, which
    publishes all container ports as host ports on a per-invocation basis.
    """

    image: str
    """The name and tag of an image on Docker Hub."""

    hostname: str
    """The hostname to use.

    By default, the name of the service is used as the hostname.
    """

    extra_hosts: List[str]
    """Additional hostname mappings."""

    entrypoint: List[str]
    """Override the entrypoint specified in the image."""

    command: List[str]
    """Override the command specified in the image."""

    init: bool
    """Whether to run an init process in the container."""

    ports: Sequence[Union[int, str]]
    """Service ports to expose to the host."""

    environment: List[str]
    """Additional environment variables to set.

    Each entry must be in the form `NAME=VALUE`.

    TODO(benesch): this should accept a `Dict[str, str]` instead.
    """

    depends_on: Union[List[str], Dict[str, ServiceDependency]]
    """The list of other services that must be started before this one."""

    tmpfs: List[str]
    """Paths at which to mount temporary file systems inside the container."""

    volumes: List[str]
    """Volumes to attach to the service."""

    networks: Dict[str, Dict[str, List[str]]]
    """Additional networks to join.

    TODO(benesch): this should use a nested TypedDict.
    """

    deploy: Dict[str, Dict[str, Dict[str, str]]]
    """Additional deployment configuration, like resource limits.

    TODO(benesch): this should use a nested TypedDict.
    """

    ulimits: Dict[str, Any]
    """Override the default ulimits for a container."""

    working_dir: str
    """Overrides the container's working directory."""

    healthcheck: ServiceHealthcheck
    """Configuration for a check to determine whether the containers for this
    service are healthy."""

    restart: str
    """Restart policy."""


class Service:
    """A Docker Compose service in a `Composition`.

    Attributes:
        name: The name of the service.
        config: The definition of the service.
    """

    def __init__(self, name: str, config: ServiceConfig) -> None:
        self.name = name
        self.config = config


class WorkflowArgumentParser(argparse.ArgumentParser):
    """An argument parser provided to a workflow in a `Composition`.

    You can call `add_argument` and other methods on this argument parser like
    usual. When you are ready to parse arguments, call `parse_args` or
    `parse_known_args` like usual; the argument parser will automatically use
    the arguments that the user provided to the workflow.
    """

    def __init__(self, name: str, description: Optional[str], args: List[str]):
        self.args = args
        super().__init__(prog=f"mzcompose run {name}", description=description)

    def parse_known_args(
        self,
        args: Optional[Sequence[str]] = None,
        namespace: Optional[argparse.Namespace] = None,
    ) -> Tuple[argparse.Namespace, List[str]]:
        if args is None:
            args = self.args
        return super().parse_known_args(args, namespace)


# TODO(benesch): replace with Docker health checks.
def _check_tcp(
    cmd: List[str], host: str, port: int, timeout_secs: int, kind: str = ""
) -> List[str]:
    cmd.extend(
        [
            "timeout",
            str(timeout_secs),
            "bash",
            "-c",
            f"until [ cat < /dev/null > /dev/tcp/{host}/{port} ] ; do sleep 0.1 ; done",
        ]
    )
    try:
        spawn.capture(cmd, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as e:
        ui.log_in_automation(
            "wait-for-tcp ({}{}:{}): error running {}: {}, stdout:\n{}\nstderr:\n{}".format(
                kind, host, port, ui.shell_quote(cmd), e, e.stdout, e.stderr
            )
        )
        raise
    return cmd


# TODO(benesch): replace with Docker health checks.
def _wait_for_pg(
    timeout_secs: int,
    query: str,
    dbname: str,
    port: int,
    host: str,
    user: str,
    password: Optional[str],
    expected: Union[Iterable[Any], Literal["any"]],
    print_result: bool = False,
    ssl_context: Optional[SSLContext] = None,
) -> None:
    """Wait for a pg-compatible database (includes materialized)"""
    obfuscated_password = password[0:1] if password is not None else ""
    args = f"dbname={dbname} host={host} port={port} user={user} password='{obfuscated_password}...'"
    ui.progress(f"waiting for {args} to handle {query!r}", "C")
    error = None
    for remaining in ui.timeout_loop(timeout_secs, tick=0.5):
        try:
            conn = pg8000.connect(
                database=dbname,
                host=host,
                port=port,
                user=user,
                password=password,
                timeout=1,
                ssl_context=ssl_context,
            )
            # The default (autocommit = false) wraps everything in a transaction.
            conn.autocommit = True
            cur = conn.cursor()
            cur.execute(query)
            if expected == "any" and cur.rowcount == -1:
                ui.progress(" success!", finish=True)
                return
            result = list(cur.fetchall())
            if expected == "any" or result == expected:
                if print_result:
                    say(f"query result: {result}")
                else:
                    ui.progress(" success!", finish=True)
                return
            else:
                say(
                    f"host={host} port={port} did not return rows matching {expected} got: {result}"
                )
        except Exception as e:
            ui.progress(f"{e if print_result else ''} {int(remaining)}")
            error = e
    ui.progress(finish=True)
    raise UIError(f"never got correct result for {args}: {error}")
