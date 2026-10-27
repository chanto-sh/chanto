# Docs

These files are the source of the docs at [chanto.sh/docs](https://chanto.sh/docs).
The website copies them from each release, so a change here goes live with the
next release.

| File | Published at |
| --- | --- |
| `index.md` | `/docs` |
| `quickstart.md` | `/docs/quickstart` |
| `models.md` | `/docs/models` |
| `faq.md` | `/docs/faq` |
| `cloud.md` | `/docs/cloud` |
| `api.md` | `/docs/api` |
| `reference.md` | `/docs/reference/*`, one page per `##` section |

`security-model.md` and `security/` aren't published on the website.

## Writing

- Keep the front matter (`title`, `section`, `order`, `description`) at the top of
  each page other than `reference.md`; the website's sidebar uses it.
- Link between docs files with relative `.md` links (`./models.md`,
  `./reference.md#sandbox`). They work as they are on GitHub, and the website
  turns them into its own links (`/docs/models`, `/docs/reference/sandbox`).
- Link to other chanto.sh pages with a full URL (`https://chanto.sh/pricing`).
- Each `##` section of `reference.md` becomes its own page.

The files are plain Markdown and read fine on GitHub.
