# Đặc tả triển khai Plugin Host cho VideoFlow · v2.2

**Repo đích:** `ngapngap/VideoFlow`. Dự thảo triển khai cho Tauri/Rust và Svelte, dựa trên VideoFlow v1.5.5 (0954f29). Đầu ra gồm registry, host, client, UI cấu hình và bộ kiểm thử hợp đồng dùng lại được. Dùng plugin giả để phát triển toàn bộ phần này độc lập với provider thật.

## Plugin Contract v1

Hai repo dùng cùng phiên bản hợp đồng này. Repo VideoFlow quản lý bộ fixture và test tương thích có version để CI của plugin chạy độc lập. Thay đổi phá tương thích cần major mới.

### Khởi động một instance

Manifest khai `id`, `name`, `version`, `plugin_api_version: "1"`, `entry.exe`, `entry.args`, `preferred_port`, `health_path`, `capabilities_path`, `quota_path`, `config_schema` và `actions[]`. ID plugin và instance khớp `^[a-z0-9-]+$`. Phiên bản core tối thiểu trong gói phát hành phải là số release thật. Host thay `{port}`, `{data_dir}`, `{config_file}` trong từng đối số. Config file là JSON của instance, chỉ chứa giá trị công khai. Host truyền Bearer ngẫu nhiên qua biến môi trường `VF_PLUGIN_TOKEN` và object JSON các field bí mật qua `VF_PLUGIN_SECRETS_JSON`. Plugin chỉ bind 127.0.0.1. Mọi route kiểm tra Bearer, trả 401 khi sai. Host chỉ gửi key tới đúng origin của instance. Không ghi token hay secret vào log.

### HTTP và dữ liệu

| Route | Yêu cầu và kết quả |
|---|---|
| GET /healthz | JSON `{status,checks,message}`. HTTP 200 cho `ok`, 503 cho `degraded` hoặc `down`. Khi degraded, dừng cấp job mới và vẫn cho poll, tải, đăng nhập hoặc đối soát. |
| GET /vf-plugin/v1/capabilities | `plugin_api_version,kinds,stale`. Mỗi kind công bố `modes,models`; mỗi model có `id,label,aspects`, video có `durations` tính bằng giây, tùy chọn `retires_on,cost_hint`. Video khai `max_concurrent_jobs_per_account,poll_hint:{interval_s,max_s},supports_cancel` (`none` hoặc `before_submit_only`). Chỉ quảng bá tính năng đã triển khai. |
| GET /vf-plugin/v1/quota | `{limits:[{media_kind,unit,accounts:[{id,label,balance,total,reset_at,status}],total_balance}]}`. Status nhận `ok, exhausted, locked, auth_required, unknown`. Dữ liệu chưa biết dùng null, gồm balance khi unknown/auth_required. |
| POST /v1/videos | JSON `{model,prompt,image,input_reference,size,seconds}`; hai field ảnh có thể mang cùng data URL của một ảnh đầu vào. Trả 202 `{id,task_id,status:"processing"}` với hai ID cùng giá trị. |
| GET /v1/videos/{id} | 200 với `status` là `processing`, `succeeded` (kèm `media_name`), `failed` (kèm error), `cancelled`, `abandoned` hoặc `reconcile_needed` (kèm error). 404 nghĩa là instance chưa tìm được ID. |
| GET /v1/media/{name} | Tên media logic do plugin cấp. Trả 200 và bytes MP4. Media đã biết nhưng hết thời hạn lưu trả 410 với kind=not_found; host giữ operation để recovery. Host dựng URL từ origin hiện tại của instance. |
| POST /v1/images/edits | Multipart gồm `image,prompt,model,n=1,response_format=b64_json`. Trả 202 với bản ghi operation khi đang chạy hoặc 200 khi thành công với `{"data":[{"b64_json":"…"}]}`. |
| DELETE /v1/videos/{id} | Trả `{status,message}`. Dùng `cancelled` khi xác nhận đã hủy trước submit, `abandoned` khi đã submit và có thể bị tính phí, giữ kết quả nếu job đã hoàn tất. Gọi lặp phải an toàn. |
| POST /vf-plugin/v1/actions/{id} | Nhận object JSON, mặc định `{}`. Trả `{status,message}`. Action video chuẩn tùy chọn `recover_media` nhận `{recoverable_media_id}`; thành công trả thêm `task_id` cho luồng poll/tải hiện có. |

**Quyền tạo lại tác vụ.** Cả hai POST tạo media bắt buộc có header `Idempotency-Key` là UUID bền vững của một thao tác. Plugin lưu key, dấu vân tay của đầu vào thực tế (gồm bytes ảnh và tùy chọn hiệu lực), trạng thái và kết quả. Cùng key khác đầu vào trả 409. Các request trùng đồng thời dùng chung một thao tác. Khi mất phản hồi, host tra UUID qua GET trên instance ban đầu. Chỉ gửi lại POST khi người dùng vẫn muốn tiếp tục, với cùng request và UUID. Sau STOP chỉ dùng DELETE và GET. Plugin chỉ được thực hiện thêm lệnh tạo ở upstream khi xác nhận lần trước chưa submit và được phép retry. Mọi kết quả chưa rõ phải giữ để đối soát. Chỉ thao tác tạo mới có chủ ý của người dùng mới cấp UUID mới.

**Lỗi.** Lỗi POST trả HTTP ngoài 2xx và body `{status:"failed",error:{code,kind,retryable,message,scope,retry_after_s?,recoverable_media_id?}}`. Poll job thất bại dùng cùng object error trong HTTP 200. Các kind là `transient,business,quota,auth,not_found,unsupported`. Core chỉ thử lại thao tác tạo khi transient và retryable=true, theo thời gian chờ nếu có. Lỗi đọc trạng thái có thể thử lại GET. Poll 404, mất kết nối, hết hạn poll hoặc job cần đối soát đều không cấp key mới. Code và message phục vụ hiển thị, message không chứa prompt, secret hay đường dẫn máy.

### Tra cứu và dừng theo UUID

| Route | Hành vi bắt buộc với cả ảnh và video |
|---|---|
| GET /vf-plugin/v1/operations/{uuid} | Chỉ đọc. Trả 404 nếu chưa có bản ghi, 200 nếu có. 404 không chứng minh POST cũ sẽ không đến. |
| DELETE /vf-plugin/v1/operations/{uuid} | Luôn hỗ trợ, kể cả khi chưa có task ID hoặc upstream không hỗ trợ hủy. Trả 200 với bản ghi operation sau khi quyết định đã được lưu. Nếu UUID chưa tồn tại, ghi dấu hủy bền vững trước khi trả lời. |

Bản ghi gồm `operation_id,status`; `media_kind` là image hoặc video, có thể thiếu khi DELETE tới trước POST. Status nhận processing, succeeded, failed, reconcile_needed, cancelled hoặc abandoned. Video đã tiếp nhận có thêm task_id; khi succeeded có thêm media_name. Ảnh succeeded chứa data[0].b64_json theo đúng định dạng của POST ảnh. failed và reconcile_needed chứa error. Khi ảnh cần đối soát mà thiếu bằng chứng hoặc file, UI giữ trạng thái cùng UUID và cho người dùng đóng việc theo dõi cục bộ. Chỉ cấp UUID khác khi người dùng chủ động chọn tạo ảnh mới.

Plugin tuần tự hóa quyết định hủy và quyết định bắt đầu submit trên cùng UUID bằng giao dịch bền vững. Nếu hủy thắng trước lúc worker nhận quyền submit, worker và mọi POST đến muộn đều bỏ qua generate. POST vào UUID đã hủy trả 409 cùng error.code=OPERATION_CANCELLED; GET vẫn trả cancelled. Nếu worker đã nhận quyền và upstream có thể đã thấy lệnh, trả abandoned khi biết đã submit, reconcile_needed khi chưa rõ. Với job terminal, DELETE trả lại trạng thái đã lưu; GET có thể trả succeeded về sau nếu tìm được media của job abandoned. Giữ bản ghi dedup và dấu hủy qua restart suốt vòng đời instance, kể cả sau khi dọn file media. Sau khi dọn file, báo kết quả cần đối soát hoặc media không còn, tránh tạo lại. DELETE video bằng task ID dùng cùng quyết định hủy này. Core lưu ý định STOP trước khi gửi DELETE, gửi lại DELETE nếu mất phản hồi hoặc instance đang offline. UI hiện “đang chờ xác nhận dừng” đến khi đọc được kết quả.

POST trùng cùng đầu vào trả cùng task ID cho video (202, sau đó GET để biết trạng thái hiện tại); ảnh đang xử lý trả 202 với operation, ảnh hoàn tất trả 200 với dữ liệu đã lưu. Nếu operation đã failed hoặc reconcile_needed và không đủ điều kiện tiếp tục, POST trả lỗi theo contract và GET trả trạng thái bền vững. Với lỗi trước submit có retryable=true, chỉ request POST chủ động tiếp tục mới có thể chuyển operation về processing. GET và DELETE không kích hoạt retry.

### Danh tính, cấu hình và phạm vi lỗi

Một instance gắn với một danh tính profile/tài khoản. Đổi profile hoặc đăng nhập tài khoản khác cần instance mới. Owner cũ và data dir cũ phải còn truy cập được để đối soát. Plugin xác định profile thực cả khi người dùng để trống tên ở lần đầu; lưu binding trong DB và kiểm tra lại khi khởi động/login. Nếu danh tính thay đổi hoặc chưa xác minh được khi resume, giữ degraded và chỉ phục vụ kết quả cục bộ hoặc đối soát an toàn. Lần lookup theo UUID đã biết đi thẳng vào DB trước khi kiểm tra readiness cho job mới. Mỗi operation lưu snapshot project, model và tùy chọn đã resolve trước submit. Khi so request trùng, dùng tham số đã gửi và snapshot gốc; các default mới chỉ áp dụng cho UUID mới.

Port ưu tiên theo thứ tự: giá trị instance `bridge_port` nếu được đặt, manifest.preferred_port, rồi port hệ điều hành cấp. Port ưu tiên bận thì host chọn port trống. Plugin bind đúng `{port}`. File `{config_file}` thuộc host và được đọc một chiều. Project do plugin tạo mặc định được lưu bền vững trong DB của instance.

Mỗi error khai `scope` là operation, media_kind hoặc instance. operation chỉ ảnh hưởng UUID hiện tại. media_kind áp dụng cho các job mới cùng loại trên instance đó; lấy loại từ request/operation. instance áp dụng cho mọi job tạo mới ở instance đó. GET, recovery và các instance khác vẫn hoạt động. retryable quyết định có được thử lại việc tạo cho UUID đó; retry_after_s độc lập, là thời gian tối thiểu tính từ khi nhận phản hồi cho phạm vi bị chặn. Khi thiếu thời hạn cho lỗi quota/auth diện rộng, chỉ mở lại sau lần kiểm tra quota/health thành công xác nhận điều kiện đã hết. Quota trả từng nhóm limits theo media_kind và unit. Không áp số dư từ nhóm video cho ảnh.

### Quy ước HTTP dùng cho hai phía

Xác thực trước khi xử lý mọi route, kể cả GET /healthz. Bearer phiên thiếu hoặc sai trả 401. Khi phiên Google hết hạn, Bearer giữa host và plugin vẫn hợp lệ. Ánh xạ dưới đây áp dụng khi request thất bại ngay tại HTTP. GET đọc được một job đã failed vẫn trả 200 với lỗi của job. Health dùng mã theo trạng thái tổng hợp như bảng route.

| Điều kiện | HTTP | kind trong error |
|---|---:|---|
| Sai Bearer phiên | 401 | auth, code PLUGIN_TOKEN_INVALID |
| Cần đăng nhập lại upstream | 424 | auth |
| Thiếu quota upstream | 429 | quota |
| Rate limit có xác nhận | 429 | transient |
| Upstream tạm mất kết nối, cooldown hoặc worker bận | 503 | transient |
| JSON/multipart sai cấu trúc, kiểu, trường xung đột | 400 | business |
| Dữ liệu đúng kiểu nhưng ngoài model/duration/aspect hoặc vượt cap | 422 | business |
| Plugin nhận diện tính năng nhưng chưa hỗ trợ | 501 | unsupported |
| ID tài nguyên/action không tồn tại, kể cả DELETE video bằng task ID | 404 | not_found |
| UUID trùng khác payload hoặc POST đến sau khi UUID đã hủy | 409 | business |

Host dùng kind, scope và retryable để quyết định xử lý. Với 401 host kiểm tra phiên process; với 424 host hiển thị action login. DELETE theo UUID **chưa tồn tại** vẫn ghi dấu hủy và trả 200 như mục tra cứu theo UUID.

**Health.** checks là object có khóa là tên phép kiểm tra ổn định và giá trị là object có status (ok, degraded hoặc down) và message dạng string. Host chấp nhận tên phép kiểm tra mới. Status cấp ngoài do plugin tổng hợp theo khả năng nhận job. Khi process không trả HTTP, host ghi nhận down cục bộ.

```json
{"status":"degraded","checks":{"http":{"status":"ok","message":"Sẵn sàng"},"account":{"status":"degraded","message":"Cần đăng nhập lại"}},"message":"Tạm dừng nhận job mới"}
```

**Video.** size là chuỗi kích thước pixel gồm hai số nguyên dương nối bằng chữ x thường, ví dụ "1280x720". seconds là số nguyên JSON dương. Chuỗi "16:9" cho size hoặc "8" cho seconds trả 400. Với i2v, gửi ít nhất một trong image và input_reference dưới dạng data URL ảnh base64. Khi có cả hai, giải mã và so bytes; khác bytes hoặc data URL hỏng trả 400 trước submit. Tỷ lệ sau rút gọn và thời lượng phải được model hỗ trợ.

**Ảnh.** Multipart có thể thêm aspect_ratio dạng chuỗi như "9:16", thuộc aspects của model. Bỏ trống dùng mặc định của plugin và lưu giá trị hiệu lực cùng operation. Không nhận field size ở route ảnh (400). Check fingerprint gồm aspect hiệu lực. Khi bytes kết quả đã lưu sẵn lúc trả lời, gửi 200 với data; khi đã tiếp nhận nhưng chưa có kết quả, gửi 202 với operation processing, kể cả POST đầu tiên hoặc bản gửi trùng. Sau 202 host dùng GET theo UUID.

**Actions.** Mỗi phần tử manifest.actions có id, label, scope (instance hoặc job, mặc định instance) và confirm tùy chọn. login và open_project nhận {}, scope=instance. Trả 202 với {"status":"started","message":"Đã mở trình duyệt"} ngay sau khi mở thành công. Đây là xác nhận đã mở, host đọc lại health để biết kết quả đăng nhập. Lỗi mở trình duyệt trả error tại HTTP. recover_media dùng scope=job; host chỉ hiện trên job của instance có recoverable_media_id. Khi đã nhận và có task để poll, trả 202 với status=started, message, task_id; nếu phục hồi đã xong, trả 200 với status=succeeded, message, task_id. Sau cả hai host poll task_id trên đúng instance.

**Config và manifest.** Tên file là manifest.json ở gốc gói. min_core_version là chuỗi SemVer bắt buộc trong gói phát hành, bằng release VideoFlow sớm nhất đã vượt suite Contract v1. Trong manifest dev/fixture, giá trị null nghĩa là chưa chốt release. Host so SemVer khi có giá trị. Host cung cấp một snapshot config cho mỗi lần spawn. Plugin đọc một lần khi khởi động. Lưu thay đổi config hoặc secret đánh dấu restart_required; host ngừng cấp việc mới, cho việc đang xử lý đến điểm an toàn, rồi spawn phiên mới với config đã ghi nguyên tử và secret mới. Nếu vẫn còn lượt upstream phụ thuộc process đang chạy thì hoãn restart. Job resume dùng snapshot của operation ban đầu.

**Kiểm thử giao diện.** Kiểm tra riêng 401 của health, 424 của POST khi upstream mất phiên, 429 quota và 429 transient, cả hai dạng DELETE ID chưa biết, ảnh trùng còn chạy → 202 và đã lưu → 200, hai ảnh video khác bytes → 400 với 0 submit. Thay config trong lúc job đang chạy phải giữ snapshot cũ đến khi job kết thúc.

### Fixture tương thích v1.2 (Contract major 1)

Hai repo có thể trích trực tiếp các JSON dưới đây. Fixture đầu là một manifest đầy đủ cho process giả; CI cung cấp executable có tên trong fixture. Fixture sau chứa các response theo tên ca test; mỗi giá trị trong object là một body HTTP riêng. Các key bao ngoài chỉ dùng để đặt tên test.

```json
{"id":"demo","min_core_version":null,"name":"Demo","version":"0.1.0","plugin_api_version":"1","entry":{"exe":"demo.exe","args":["--port","{port}","--data-dir","{data_dir}","--config-file","{config_file}"]},"preferred_port":18611,"health_path":"/healthz","capabilities_path":"/vf-plugin/v1/capabilities","quota_path":"/vf-plugin/v1/quota","config_schema":{"type":"object","properties":{"video_model":{"type":"string","default":"m1","x-dynamic-enum":"capabilities.video.models[].id"}}},"actions":[]}
```

```json
{"capabilities":{"plugin_api_version":"1","kinds":["video","image"],"stale":false,"video":{"modes":["i2v"],"models":[{"id":"m1","label":"Demo video","aspects":["16:9"],"durations":[8]}],"max_concurrent_jobs_per_account":1,"poll_hint":{"interval_s":15,"max_s":1800},"supports_cancel":"before_submit_only"},"image":{"modes":["i2i"],"models":[{"id":"i1","label":"Demo image","aspects":["1:1"]}]}},"quota":{"limits":[{"media_kind":"video","unit":"credits","accounts":[{"id":"default","label":"A","balance":0,"total":100,"reset_at":null,"status":"exhausted"}],"total_balance":0}]},"video_post_replay_202":{"id":"t1","task_id":"t1","status":"processing"},"video_poll_200":{"status":"succeeded","media_name":"clip.mp4"},"image_pending_202":{"operation_id":"123e4567-e89b-42d3-a456-426614174000","media_kind":"image","status":"processing"},"image_lookup_200":{"operation_id":"123e4567-e89b-42d3-a456-426614174000","media_kind":"image","status":"succeeded","data":[{"b64_json":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGNgAAAAAgABSK+kcQAAAABJRU5ErkJggg=="}]},"cancel_before_post_200":{"operation_id":"123e4567-e89b-42d3-a456-426614174001","status":"cancelled"},"cooldown":{"status":"failed","error":{"kind":"transient","code":"PROVIDER_RATE_LIMITED","scope":"instance","retryable":false,"retry_after_s":1800,"message":"Tạm dừng nhận việc trên instance này."}}}
```

Test bắt buộc trên cùng fixture: DELETE trước POST và restart rồi POST muộn → 0 generate; POST đã submit trước DELETE → tối đa 1 generate; hai POST ảnh đồng thời rồi mất phản hồi → GET trả ảnh đã lưu, tổng cộng 1 generate. Đổi model/project sau submit vẫn lấy đúng kết quả gốc. Đổi profile tạo instance mới, B không nhận job cũ của A. Cooldown instance A vẫn cho B chạy; quota video cạn vẫn cho tạo ảnh nếu hạn mức ảnh cho phép.

## Registry và vòng đời process

Trong `videoflow-app/src-tauri/src/plugins/`, tách manifest, registry, config, host và client. Quét thư mục plugin đi kèm từ resource directory thực tế của bản cài và plugin người dùng tại `%APPDATA%/VideoFlowDesktop/plugins/<id>/`. Từ chối id trùng, major không hỗ trợ, thiếu executable hoặc đường dẫn thoát khỏi thư mục gói. Kiểm tra min_core_version khi manifest khai báo. Một manifest hỏng chỉ vô hiệu hóa gói đó.

Plugin cài thêm cần người dùng tin tưởng trước lần spawn. Gắn quyết định tin tưởng với nội dung gói hoặc chữ ký và quyền đã duyệt; khi danh tính gói/quyền thay đổi phải duyệt lại. Quyền khai trong manifest là thông tin cho người dùng; process Windows chạy với quyền của tài khoản. Chỉ tin mặc định các gói thuộc bản phân phối ứng dụng.

Mỗi cặp plugin/instance có một process, key phiên và data dir riêng tại `plugins-data/<id>/<instance>/`. Serialize start/stop đồng thời. Thử port ưa thích, nếu bận chọn port khả dụng, dành riêng 18610 cho Muse. Chỉ công nhận process sau khi xác thực phiên và đọc health. Chờ tối đa 90 giây; giữ instance degraded để còn login, poll và recovery. Chỉ thu hồi process do host quản lý và xác định đúng instance. Khi config đổi, báo cần restart và giữ khả năng khôi phục job đang chạy. Xóa data dir là thao tác riêng có xác nhận.

## Cấu hình và giao diện

Lưu enabled, trust và config công khai theo khóa `<id>:<instance>` trong plugins.json. Field x-secret=true hoặc format=password vào Windows Credential Manager với khóa theo plugin, instance và field. Trả trạng thái đã cấu hình cho UI, tránh ghi lại giá trị trống khi người dùng chỉ sửa field khác. Schema v1 hỗ trợ string, enum, x-dynamic-enum, integer min/max, boolean và secret. Kiểu chưa hỗ trợ phải có thông báo. Đọc video_model/image_model của instance khi tạo request. UI lấy danh sách model, duration, aspect và ngày retire từ capabilities; cảnh báo model sắp nghỉ trong 7 ngày. Cache theo instance, thể hiện dữ liệu stale, để plugin kiểm tra cuối cùng lúc submit.

Thêm lệnh Tauri list, thêm/xóa instance, trust/enable, start/stop/status, capabilities/quota, save_config và action; mọi lệnh trên instance phải nhận cả plugin ID và instance ID. Action recovery nhận cả payload. Đăng ký lệnh ở entry point Tauri và cập nhật kiểu ở frontend. Settings render schema và action theo manifest. Dashboard, trạng thái khởi động app và màn hình dòng dữ liệu phải nhận biết nguồn plugin khi người dùng chưa lưu gateway key. Hiển thị lý do degraded, quota chưa biết, cooldown, job cần đối soát và nút lấy lại clip khi có ID.

## Chọn nguồn và cấp worker

Áp dụng cho video_source và image_source. Giữ đường dẫn gateway/internal và cách chuẩn hóa cấu hình cũ. `plugin:<id>` là instance default; `plugin:<id>:<instance>` ghim vào một instance; `plugin-pool:<id>` cho phép tự phân bổ giữa các instance đã bật. Instance hợp lệ nhưng mất kết nối phải báo unavailable. Form chọn nguồn phải thể hiện rõ chế độ. Khi load Settings chưa có capabilities, không sửa duration của nguồn plugin dựa trên model đoán trước.

Scheduler chỉ cộng slot của các instance thuộc nguồn đã chọn và đang sẵn sàng. Với pool, chọn một slot và gắn owner trước POST. Cả worker ảnh lẫn video dùng chung bộ đếm khi tranh cùng tài khoản/instance. Với provider một slot, giữ slot suốt thời gian job chiếm tài nguyên, kể cả lúc chuyển từ submit sang poll. Giữ thứ tự của dòng nối cảnh. Khi hết slot, xếp hàng. Khi một instance bị cooldown/hết hạn mức, ngừng cấp việc phù hợp cho nó; vẫn theo dõi job cũ và tiếp tục các nguồn còn dùng được. Không đổi owner của thao tác đang dở.

## Dữ liệu bền vững, retry và STOP

Mở rộng migration SQLite trong `src-tauri/src/state_db.rs` và đường resume trong `engine/planner.rs`. Trước POST đầu của B2 hoặc B3, commit UUID thao tác, loại thao tác, owner, tham số hiệu lực/dấu vân tay và liên kết dòng. Lưu task ID, media_name hoặc recovery ID và trạng thái khi biết. UUID thuộc thao tác và có thể đi qua nhiều run; thao tác chủ động tạo lại hoặc đổi input tạo UUID mới. Khi đọc job cũ chưa có metadata, giữ đường resume legacy và báo cần đối soát nếu không thể xác định owner của job plugin. Lỗi ghi DB trước submit phải dừng gửi.

Trong `adapters.rs`, dispatch ở cả create, poll, download, image edit và kiểm tra kết nối. Poll/tải của job đã lưu dùng owner đã ghi dù Settings thay đổi. Với plugin, lưu locator logic và dựng lại URL qua host; không phụ thuộc port cũ sau restart. Kiểm tra same-origin trước khi gắn Bearer. Trong `engine.rs` và pipeline, dùng một giá trị retryable nhất quán cho mỗi lỗi. Áp dụng cho cả retry B2, create B3, nhánh poll lỗi và nhánh tự tạo lại sau 404. Với plugin, giữ job chưa rõ kết quả để đối soát, bảo toàn ID khi dừng và phân biệt retry GET với POST. Ở B2, đọc response 202 và dùng GET operation đến khi có ảnh hoặc trạng thái terminal. Với lỗi diện rộng, scheduler áp scope và thời hạn của error. Giới hạn thời gian poll theo hint; hết thời gian chỉ dừng chờ.

STOP ghi ý định dừng và chặn cấp việc mới, gửi DELETE theo operation UUID về đúng owner, kể cả khi chưa nhận task ID. Sau restart tiếp tục gửi các yêu cầu hủy chưa được xác nhận. UI phân biệt đang chờ xác nhận, cancelled, abandoned và reconcile_needed. Nguồn gateway và Muse giữ hành vi dừng hiện có.

## Bàn giao và nghiệm thu bằng plugin giả

1. **Framework.** Cài một gói giả ngoài repo core. Kiểm tra quét manifest, trust, health, Bearer khác nhau từng instance, port bận, API major lạ, process chết và JSON hỏng. Chạy create → poll → download và tải đúng MP4 khi restart sang port khác.
2. **Khôi phục.** Cắt phản hồi POST đã thành công cho B2 và B3, restart core rồi resume. Đếm đúng một lần generate. Tạo hai project cùng số run/job và prompt nhưng ảnh khác phải sinh hai thao tác riêng. Thử cùng key khác payload, đồng thời hai request, poll 404 và failed/retryable=false; số lần submit upstream phải đúng theo hợp đồng.
3. **Hai instance.** Chạy hai job trong pool, mỗi instance một slot. Ghim A thì chỉ A nhận việc. Đổi Settings sang B hoặc tắt A sau submit, kể cả khi hai instance cấp cùng task ID, job cũ vẫn thuộc A. Thử cùng lúc B2/B3, cooldown A, STOP trước/sau submit và phục hồi media theo ID.
4. **UI.** Plugin giả thêm model qua capabilities thì dropdown cập nhật sau lần refresh/start; manifest thêm field schema được hỗ trợ thì UI render được. Kiểm tra field secret vắng khỏi plugins.json, file config và log; test health degraded vẫn thao tác login/recovery được.
5. **Tương thích.** Chạy suite Rust/frontend và migration trên DB v1.5.5. Chạy lại luồng gateway/internal, bao gồm retry, STOP, duration và nối cảnh. Mã production xử lý theo contract, kiểm tra bằng fixture plugin có ID/model bất kỳ. Phát hành bộ fixture có version cùng contract để repo plugin pin trong CI.

Giai đoạn đầu hoàn thành framework và luồng video bằng provider giả. Renderer có thể triển khai song song. Các tính năng extend, scene merge, upscale và character cần có thao tác chuẩn và test tương thích trước khi UI cho gọi. Phạm vi của repo core dừng ở hợp đồng chung và bộ test.
