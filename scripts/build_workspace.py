#!/usr/bin/env python3
"""Establish a trusted filesystem namespace before build tools write or sign.

The application and compiler caches remain reusable. Existing untrusted paths
are rejected, never adopted by changing their owner or permissions.
"""
import argparse
import ctypes
import errno
import os
from pathlib import Path
import stat
import sys
import tempfile


class UnsafeWorkspace(ValueError):
    pass


def _check_acl(descriptor, path):
    if sys.platform != 'darwin':
        return
    # Public Darwin ACL API. Read the opened object, not its mutable pathname.
    library = ctypes.CDLL(None, use_errno=True)
    pointer = ctypes.c_void_p
    library.acl_get_fd.argtypes = [ctypes.c_int]
    library.acl_get_fd.restype = pointer
    library.acl_get_entry.argtypes = [pointer, ctypes.c_int, ctypes.POINTER(pointer)]
    library.acl_get_tag_type.argtypes = [pointer, ctypes.POINTER(ctypes.c_int)]
    library.acl_get_permset_mask_np.argtypes = [pointer, ctypes.POINTER(ctypes.c_uint64)]
    library.acl_free.argtypes = [pointer]
    acl = library.acl_get_fd(descriptor)
    if not acl:
        error = ctypes.get_errno()
        if error in (errno.ENOENT, getattr(errno, 'ENOATTR', 93)):
            return
        raise OSError(error, os.strerror(error), str(path))
    # sys/acl.h: WRITE_DATA, DELETE, APPEND_DATA, DELETE_CHILD,
    # WRITE_SECURITY and CHANGE_OWNER can change this namespace or its access.
    mutation_permissions = sum(1 << bit for bit in (2, 4, 5, 6, 12, 13))
    try:
        entry = pointer()
        selector = 0  # ACL_FIRST_ENTRY
        while library.acl_get_entry(acl, selector, ctypes.byref(entry)) == 0:
            selector = -1  # ACL_NEXT_ENTRY
            tag, permissions = ctypes.c_int(), ctypes.c_uint64()
            if library.acl_get_tag_type(entry, ctypes.byref(tag)) != 0 or \
                    library.acl_get_permset_mask_np(entry, ctypes.byref(permissions)) != 0:
                raise UnsafeWorkspace(f'Cannot verify workspace ACL: {path}')
            if tag.value == 1 and permissions.value & mutation_permissions:
                raise UnsafeWorkspace(f'Workspace has an allow ACL granting mutation: {path}')
    finally:
        library.acl_free(acl)


def _check_object(descriptor, path, *, shared_parent=False):
    metadata = os.fstat(descriptor)
    if metadata.st_uid not in (0, os.getuid()):
        raise UnsafeWorkspace(f'Workspace component belongs to another user: {path}')
    writable_by_others = metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH)
    sticky_parent = shared_parent and metadata.st_uid == 0 and metadata.st_mode & stat.S_ISVTX
    if writable_by_others and not sticky_parent:
        raise UnsafeWorkspace(f'Workspace component is writable by other users: {path}')
    _check_acl(descriptor, path)
    return metadata


def _system_alias(path):
    """Permit only verified root-owned macOS aliases, before walking with fds."""
    for alias, target in (('/tmp', '/private/tmp'), ('/var', '/private/var'), ('/etc', '/private/etc')):
        if str(path) == alias or str(path).startswith(alias + '/'):
            metadata = os.lstat(alias)
            resolved = os.path.normpath(os.path.join(os.path.dirname(alias), os.readlink(alias))) if stat.S_ISLNK(metadata.st_mode) else None
            if metadata.st_uid == 0 and resolved == target:
                return Path(target) / path.relative_to(alias)
    return path


def _check_contents(descriptor, path):
    # This covers existing bundle/slice output. It is metadata-only; contents
    # are not read and Cargo's separate dependency cache is not traversed.
    with os.scandir(descriptor) as entries:
        for entry in entries:
            metadata = entry.stat(follow_symlinks=False)
            if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
                raise UnsafeWorkspace(f'Unexpected workspace entry or symlink: {path / entry.name}')
            flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
            if stat.S_ISDIR(metadata.st_mode):
                flags |= os.O_DIRECTORY
            child = os.open(entry.name, flags, dir_fd=descriptor)
            try:
                opened = _check_object(child, path / entry.name)
                if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
                    raise UnsafeWorkspace(f'Workspace changed during validation: {path / entry.name}')
                if stat.S_ISDIR(opened.st_mode):
                    _check_contents(child, path / entry.name)
            finally:
                os.close(child)


def prepare_directory(path, *, inspect_contents=False):
    path = _system_alias(Path(os.path.abspath(path)))
    if path == Path('/'):
        raise UnsafeWorkspace('The filesystem root cannot be a build workspace')
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    descriptor = os.open('/', flags)
    current = Path('/')
    try:
        _check_object(descriptor, current, shared_parent=True)
        for component in path.parts[1:]:
            try:
                child = os.open(component, flags, dir_fd=descriptor)
            except FileNotFoundError:
                os.mkdir(component, mode=0o700, dir_fd=descriptor)
                child = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
            current /= component
            _check_object(descriptor, current, shared_parent=current != path)
        if os.fstat(descriptor).st_uid != os.getuid():
            raise UnsafeWorkspace(f'Build workspace must belong to the current user: {path}')
        if inspect_contents:
            _check_contents(descriptor, path)
    finally:
        os.close(descriptor)
    return path


def workspace(kind, override=None):
    if kind not in ('build', 'release'):
        raise ValueError('Unknown workspace kind')
    # macOS supplies a private per-user temporary directory; the descriptor
    # walk also rejects unsafe TMPDIR overrides instead of trusting that name.
    requested = override or Path(tempfile.gettempdir()) / ('APFSearch-' + kind)
    return prepare_directory(requested, inspect_contents=kind == 'build')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('kind', choices=['build', 'release', 'directory'])
    parser.add_argument('path', nargs='?')
    options = parser.parse_args()
    try:
        if options.kind == 'directory':
            if not options.path:
                parser.error('directory requires a path')
            result = prepare_directory(options.path)
        else:
            result = workspace(options.kind, options.path or os.environ.get('APFSEARCH_BUILD_DIR'))
        print(result)
    except (OSError, ValueError) as error:
        parser.exit(1, f'Unsafe build workspace: {error}\n')
