# Trace 렌더 규격 — D2 타임라인 구현 참조 (gateway)

> **이 문서는 규칙을 만들지 않는다.** 모든 렌더 규칙의 단일 출처는
> `.agent-team/07-design/` 문서들이다. 여기는 (1) 각 규칙의 **출처로의 참조**,
> (2) 게이트웨이가 그 규칙을 **어떻게 구현했는가의 대응표**, (3) 계약과 구현
> 사이에서 발견한 **간극 기록**만 담는다. 07-design 내용을 복제하지 않는다 —
> 이중 출처는 금지(ETHOS 원칙 3)이므로, 규칙이 바뀌면 07-design이 바뀌고 이
> 문서의 참조는 그대로 유효해야 한다.

구현: `gateway/src/ui/mod.rs` (서버 렌더) · `gateway/src/ui/assets/hesmos.js`
(라이브 추가) · `gateway/src/core_port.rs` (와이어 타입) · 검증:
`gateway/tests/e2e.rs`.

---

## 1. 단일 출처 지도

| 주제 | 규칙의 원천 |
|---|---|
| 이벤트 행 해부 (seq·T+·event·details) | components.md **P-02** |
| 외부 호출 접기 행 | components.md **P-03** (W3-4 확정) |
| D2 스트림 행 (커밋 마커 포함) | components.md **P-10** |
| 렌더 규칙 전체 (CLI `trace show`와 동일) | ui-spec **§4.2** |
| 필터 패밀리와 접기 비활성 | ui-spec **§4.3** |
| seal·감사 라인 | ui-spec **§4.5** |
| 기계 어휘 (`session.open`, `gate.pass` …) | tokens.md **§2.4** (변경·번역 금지) |
| 수치 표기 (해시 8+…, 3자리 쉼표, tabular-nums) | tokens.md **§1.4** |
| 대시보드 색·타이포·간격·모션 토큰 | tokens.md **§3** |
| 커밋 마커 N의 정의 (N = CommitSeq) | W3-3 결정 (story-W3 부록) |
| HTTP-2 이벤트 페이로드·페이지네이션 | api-contracts.md **§7** HTTP-2, **TYPE-3** |
| HTTP-3 스트리밍 (구독 시점만) | api-contracts.md **§7** HTTP-3, A-N5 |
| 4화면 범위·기술 수준 | ui-spec **§9.0** |

## 2. 행 렌더 (P-02 → `row_html`)

- 열 구성과 순서는 P-02 그대로: `seq · T+ · event · details`. T+는
  `T+{초}.1f` 형식(§4.2), event는 기계 어휘 그대로(§1.2 — 사람 언어 번역 없음).
- details의 kind별 키→라벨 대응(`node_count`→`nodes`, `reason_code`→`reason` 등)은
  구현 편의 대응표이며 값은 절대 재해석하지 않는다. 소스: `ui/mod.rs::attr_label`.
- 수치는 `class="num"` + CSS `font-variant-numeric: tabular-nums`(tokens §3.2).
- 검증: `d2_render_rules_fold_commit_and_404`.

## 3. 커밋 마커 `→ commit #N` (P-10 · W3-3)

- **정의**: N = CommitSeq (W3-3). 마커는 이벤트가 아니라 **표시 전용 주석**이다 —
  게이트 판정 행(`gate.pass` 등)이 커밋 지점에 도달했음을 나타낸다.
- **와이어 계약과의 관계**: TYPE-3에는 커밋 필드가 없어 HTTP 응답에 함부로
  넣을 수 없다. 그래서 게이트웨이 와이어 타입 `TraceEventWire`에
  `commit_seq: Option<u64>`(표시 전용, 체인 해시 입력 아님 — TYPE-3 불변 1과
  무관)을 두고, **코어 어댑터가 WAL 영수증에서 채워 넣는다**. 이 필드는
  `api-contracts.md` 개정 대상이며 James 승인 전까지 "게이트웨이 확장 제안"
  상태다. → §8 간극 대장.
- 접힘 뒤에는 마커가 남지 않는다(접힌 행에는 details 한 줄만 — P-03).
- 검증: 같은 테스트가 `→ commit #1` 행을 단정한다.

## 4. 외부 호출 접기 (P-03 · W3-4)

- 대상: 같은 노드 스팬 안에서 구조 행(세션·플랜·게이트·핸드오프·노드·예산·seal)
  이 나오기 전까지 연속한 `llm.call`/`tool.call` 전부. 건수는 종류별,
  시간은 **외부 호출 누적 합계(llm+tool 모두)**.
- 형식: `(llm.call 3 · tool.call 1 생략 — 12.1s)` — seq 칸에 `⋮`, T+ 칸은 비움(P-03 예제와 동일).
- **필터 활성 시 접지 않는다**(ui-spec §4.3 — 필터 중 접음은 정보 은님이다).
- 구현: 서버 렌더는 구조 행 직전에 접기 행을 flush(`render_rows`), 라이브는
  JS가 동일 규칙으로 유지(`hesmos.js`의 `pending`/`flushFold`). 양쪽이 규칙을
  이중 구현하는 이유는 §9.0(서버 렌더 우선, JS는 보강 전용)이고, 규칙 일치는
  위 테스트가 양쪽 문자열을 단정한다.
- 검증: `d2_render_rules_fold_commit_and_404` — fold 문자열 + `filter=gate`
  에서 `생략` 부재를 동시에 단정.

## 5. 필터 (ui-spec §4.3)

- 패밀리: `gate` / `handoff` / `budget` (CLI-2 표 8의 대시보드 부분집합).
- 필터는 **GET 폼 재요청**으로 서버에서 다시 그린다(초기 페이지를 JS로 거르지
  않는다 — no-JS에서도 필터가 동작해야 하고, 접기 비활성 규칙이 서버에 있기
  때문). 라이브 추가 이벤트만 JS가 같은 패밀리 규칙으로 고른다.
- D1의 상태·팀 필터는 반대로 **클라이언트 전용**이다: HTTP-1에 필터 파라미터가
  없어 서버 필터를 만들면 계약 확장이 되기 때문(§9.2).

## 6. seal·감사 라인 (ui-spec §4.5)

- `trace.seal` 행 details에 `bathos_audit=verified|FAILED`를 붙인다.
- 값의 출처는 **HTTP-5 패스스루**(`PORT-2` 감사 결과)뿐이다. 게이트웨이는
  체인을 재검증하지 않는다(SS-24 · D-6) — 검증: `static_boundary_no_outbound_clients_or_engine_calls`.
- D4 감사 화면도 같은 출처에서 목록을 만든다(§9.5).

## 7. 라이브 동작 (HTTP-3 · A-N5) — `hesmos.js`

- **구독 시점 이후만** 스트림으로 온다(백필 금지). 서버 렌더 초기 페이지 +
  `?after=<마지막 seq>` 캐치업이 진실의 원천.
- 끊김 복구는 **전체 재동기화 후 재구독**(A-N5): 1초 고정 대기 →
  `/events?after=<마지막 seq>` → 다시 WS. 지수 백오프/지터 없음(결정론).
  화면에는 `재연결 중` 배지, 성공 시 `LIVE`.
- 이벤트 컨테이너는 `aria-live="polite"`(tokens §3 · §9.3). 자동 스크롤은
  사용자가 맨 아래에 있을 때만 고정한다.
- 캐치업 쿼리의 `after` 의미론: **없음/빈 값 = seq 0부터 전부**,
  `after=N = N 초과부터`(배타). 초기 로드가 `session.open`(seq 0)을 놓치지
  않으려면 이 구분이 필요하다. 검증: `http2_events_pagination_and_limit_guard`.

## 8. 간극 대장 (계약 ↔ 구현 — 리드 승인 대기)

| # | 간극 | 처리 | 상태 |
|---|---|---|---|
| G-1 | TYPE-3에 커밋 정보 없음 → P-10 마커 불가 | `commit_seq` 표시 전용 와이어 필드 제안 (§3) | James 검토 대기 |
| G-2 | HTTP-1에 예산 limit 없음 → D1 게이지 불가 | D1은 `budget_spent` 절대값만 표시 (J-1 = 계약 필드) | 확인된 설계 준수 |
| G-3 | `?after=` 연속 링크 라벨이 목안의 "이전"과 충돌 | "…이후 이벤트 500건 더 불러오기" 사용 | Jonnathan 검토 대기 |
| G-4 | 404/500용 TYPE-7 코드 어휘 미정 | `SESSION_NOT_FOUND`(Compile 띠, exit 3 선례)·`E-INTERNAL`(E-* 문법) 사용 | James 검토 대기 |

## 9. 바뀌면 안 되는 것 (요약)

이 문서·구현 어느 쪽도 다음을 바꿀 수 없다: 기계 어휘 목록(tokens §2.4),
상태 칩의 기호+단어+색 3중 부호화(tokens §2.1), 3색 규칙, NO_COLOR는 CLI 전용
이며 대시보드는 항상 토큰 색(tokens §3.1), 모션 0(tokens §3.4), 조회 전용
경계(ui-spec §9.0 — 변조 엔드포인트 없음, 검증: `mutating_methods_are_rejected_405`).
