"""JSON Schema 2020-12 errors of a value, for the keywords the contracts the tests check use (C1's packet and C4G's
TurnRequest); another keyword is an error."""
import re

ANNOTATIONS = {'$schema', '$id', '$defs', 'title', 'description', 'format', 'default', 'then', 'else'}
TYPES = {
    'object': lambda v: isinstance(v, dict),
    'array': lambda v: isinstance(v, list),
    'string': lambda v: isinstance(v, str),
    'boolean': lambda v: isinstance(v, bool),
    'integer': lambda v: isinstance(v, int) and not isinstance(v, bool),
    'number': lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
}


def violations(value, schema, root, at='$'):
    """The errors of `value` against `schema`, with `$ref` resolved from `root`; [] when it is valid."""
    out = []
    for key, rule in schema.items():
        if key in ANNOTATIONS or key.startswith('x-'):
            continue
        if key == '$ref':
            target = root
            for part in rule.removeprefix('#/').split('/'):
                target = target[part]
            out += violations(value, target, root, at)
        elif key == 'type':
            if not TYPES[rule](value):
                out.append(f'{at}: not {rule}')
        elif key == 'required':
            out += [f'{at}: missing {k}' for k in rule if isinstance(value, dict) and k not in value]
        elif key == 'properties':
            for name, sub in rule.items():
                if isinstance(value, dict) and name in value:
                    out += violations(value[name], sub, root, f'{at}.{name}')
        elif key == 'additionalProperties':
            if rule is not False:
                raise NotImplementedError(f'additionalProperties {rule!r}')
            extra = set(value) - set(schema.get('properties', {})) if isinstance(value, dict) else set()
            out += [f'{at}: unexpected {k}' for k in sorted(extra)]
        elif key == 'items':
            for i, item in enumerate(value if isinstance(value, list) else []):
                out += violations(item, rule, root, f'{at}[{i}]')
        elif key in ('minItems', 'maxItems'):
            if isinstance(value, list) and (len(value) < rule if key == 'minItems' else len(value) > rule):
                out.append(f'{at}: {key} {rule}')
        elif key == 'allOf':
            for sub in rule:
                out += violations(value, sub, root, at)
        elif key == 'oneOf':
            if sum(not violations(value, sub, root, at) for sub in rule) != 1:
                out.append(f'{at}: not exactly one of {len(rule)} schemas')
        elif key == 'if':
            branch = schema.get('else' if violations(value, rule, root, at) else 'then', {})
            out += violations(value, branch, root, at)
        elif key == 'const':
            if value != rule:
                out.append(f'{at}: {value!r} is not {rule!r}')
        elif key == 'enum':
            if value not in rule:
                out.append(f'{at}: {value!r} not in {rule}')
        elif key == 'pattern':
            if isinstance(value, str) and not re.search(rule, value):
                out.append(f'{at}: does not match {rule}')
        elif key in ('minLength', 'maxLength'):
            if isinstance(value, str) and (len(value) < rule if key == 'minLength' else len(value) > rule):
                out.append(f'{at}: {key} {rule}')
        elif key in ('minimum', 'maximum'):
            if TYPES['number'](value) and (value < rule if key == 'minimum' else value > rule):
                out.append(f'{at}: {key} {rule}')
        else:
            raise NotImplementedError(f'keyword {key} at {at}')
    return out
