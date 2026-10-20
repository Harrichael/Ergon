-- Data written by ergon at schema v1 (commit 0449d3e), for migration tests:
-- two projects (one retired), a Claude session, a dependency, a deleted task,
-- and creation times out of id order.
INSERT INTO projects VALUES(1,'9efdb92a-2621-462e-8323-af3096650315','alpha',1760000000,NULL);
INSERT INTO projects VALUES(2,'4a2fc1df-c301-4db5-9a92-42e42523255b','beta',1760000000,1760000000);
INSERT INTO tasks VALUES(1,1,'root',NULL,'Alpha project\n\nContext.','in_progress',NULL,NULL,NULL,1760000000,1760000000,NULL);
INSERT INTO tasks VALUES(2,1,'T4AL',1,'Backend','todo',1,NULL,NULL,1760000000,1760000000,NULL);
INSERT INTO tasks VALUES(3,1,'6Ahd',2,'API','in_progress',NULL,'fbb3e087-328f-4664-a54e-14022a4871d0','/home/user/alpha',1760000000,1760000000,NULL);
INSERT INTO tasks VALUES(4,1,'gJmw',1,'Frontend','todo',NULL,NULL,NULL,1760000500,1760000500,NULL);
INSERT INTO tasks VALUES(5,1,'fEmH',2,'Docs',NULL,NULL,NULL,NULL,1760000100,1760000100,NULL);
INSERT INTO tasks VALUES(6,1,'aAQk',4,'Old idea',NULL,NULL,NULL,NULL,1760000000,1760000000,1760000000);
INSERT INTO tasks VALUES(7,2,'root',NULL,'Beta','in_progress',NULL,NULL,NULL,1760000000,1760000000,NULL);
INSERT INTO tasks VALUES(8,2,'T4AL',7,'Only task','todo',NULL,NULL,NULL,1760000000,1760000000,NULL);
INSERT INTO task_deps VALUES(4,3);
